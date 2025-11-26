package app

import (
	"bytes"
	"context"
	"fmt"
	"log/slog"
	"net/http"
	"regexp"
	"slices"
	"strings"
	"sync"

	"github.com/goccy/go-json"
	"github.com/gofiber/fiber/v2"
	"github.com/google/uuid"
	"github.com/klauspost/compress/zstd"
	"github.com/patrickmn/go-cache"
)

var (
	blockedFields   = []string{"country"}
	zstdDecoderPool = sync.Pool{
		New: func() any {
			d, _ := zstd.NewReader(nil)
			return d
		},
	}
)

func (state *AppState) CollectHandler(c *fiber.Ctx) error {
	response, code := state.handleRequest(c)
	return c.Status(code).JSON(response)
}

func (state *AppState) handleRequest(c *fiber.Ctx) (ApiResponse, int) {
	dataReq, err := parseRequestBody(c)
	if err != nil {
		return errorResponse("Validation Error", "Invalid JSON body", []string{err.Error()}), http.StatusBadRequest
	}

	token := extractToken(c)
	if token == "" {
		return errorResponse("Unauthorized", "Invalid or missing authentication token", nil), http.StatusUnauthorized
	}

	projectCtx, err := state.getProjectContext(c.Context(), token)
	if err != nil {
		slog.Error("Failed to get project context", "error", err)
		return errorResponse("Internal Server Error", "An unexpected error occurred", nil), http.StatusInternalServerError
	}
	if projectCtx == nil {
		return errorResponse("Unauthorized", "Invalid or missing authentication token", nil), http.StatusUnauthorized
	}

	filteredData, validationErrors := processData(c, projectCtx, dataReq.Data)
	if len(validationErrors) > 0 {
		return errorResponse("Validation Error", "Validation failed for some fields", validationErrors), http.StatusBadRequest
	}

	if len(filteredData) == 0 {
		return successResponse("No data to insert"), http.StatusOK
	}

	dataJSON, err := json.Marshal(filteredData)
	if err != nil {
		slog.Error("Failed to marshal filtered data", "error", err)
		return errorResponse("Internal Server Error", "An unexpected error occurred", nil), http.StatusInternalServerError
	}

	_, err = state.Pool.Exec(c.Context(),
		"INSERT INTO data_entries (project_id, server_id, data) VALUES ($1, $2, $3)",
		projectCtx.Project.ID, dataReq.ServerID, dataJSON)
	if err != nil {
		slog.Error("Failed to insert data", "error", err)
		return errorResponse("Internal Server Error", "An unexpected error occurred", nil), http.StatusInternalServerError
	}

	return ApiResponse{Success: true}, http.StatusOK
}

func parseRequestBody(c *fiber.Ctx) (*DataRequest, error) {
	var dataReq DataRequest

	if c.Get("content-encoding") != "zstd" || c.Get("content-type") != "application/octet-stream" {
		return &dataReq, json.Unmarshal(c.Body(), &dataReq)
	}

	decoder := zstdDecoderPool.Get().(*zstd.Decoder)
	defer zstdDecoderPool.Put(decoder)

	if err := decoder.Reset(bytes.NewReader(c.Body())); err != nil {
		return nil, fmt.Errorf("failed to decompress: %w", err)
	}

	return &dataReq, json.NewDecoder(decoder).Decode(&dataReq)
}

func processData(c *fiber.Ctx, projectCtx *ProjectContext, data DataMap) (DataMap, []string) {
	filtered := make(DataMap, len(data))
	var errors []string

	if config, ok := projectCtx.ValidDataSources["country"]; ok {
		if country := getCountryFromHeaders(c); country != "" {
			if err := validateField("country", country, config); err == nil {
				filtered["country"] = country
			}
		}
	}

	for key, value := range data {
		if slices.Contains(blockedFields, key) {
			continue
		}
		config, ok := projectCtx.ValidDataSources[key]
		if !ok {
			continue
		}
		if err := validateField(key, value, config); err != nil {
			errors = append(errors, err.Error())
			continue
		}
		filtered[key] = value
	}

	return filtered, errors
}

func extractToken(c *fiber.Ctx) string {
	authHeader := c.Get("Authorization")
	if authHeader == "" {
		return ""
	}
	return strings.TrimSpace(strings.TrimPrefix(authHeader, "Bearer "))
}

func (state *AppState) getProjectContext(ctx context.Context, token string) (*ProjectContext, error) {
	if cached, found := state.Cache.Get(token); found {
		return cached.(*ProjectContext), nil
	}

	rows, err := state.Pool.Query(ctx, `
		SELECT p.id, d.reference_id, d.data_type, d.regex 
		FROM project p 
		LEFT JOIN data_sources d ON d.project_id = p.id 
		WHERE p.token = $1`, token)
	if err != nil {
		return nil, err
	}
	defer rows.Close()

	var projectID uuid.UUID
	dataSources := make(map[string]DataSourceConfig)

	hasRows := false
	for rows.Next() {
		var pid uuid.UUID
		var refID, dType, regexStr *string

		if err := rows.Scan(&pid, &refID, &dType, &regexStr); err != nil {
			return nil, err
		}
		hasRows = true
		projectID = pid

		if refID == nil || dType == nil {
			continue
		}

		var r *regexp.Regexp
		if regexStr != nil && *regexStr != "" {
			r, err = regexp.Compile(*regexStr)
			if err != nil {
				slog.Warn("Invalid regex for data source", "reference_id", *refID, "error", err)
			}
		}
		dataSources[*refID] = DataSourceConfig{DataType: *dType, Regex: r}
	}

	if !hasRows {
		return nil, nil
	}

	pCtx := &ProjectContext{
		Project:          Project{ID: projectID},
		ValidDataSources: dataSources,
	}
	state.Cache.Set(token, pCtx, cache.DefaultExpiration)

	return pCtx, nil
}

func validateField(key string, value interface{}, config DataSourceConfig) error {
	var valid bool
	switch config.DataType {
	case "string":
		_, valid = value.(string)
	case "number":
		_, valid = value.(float64)
	case "boolean":
		_, valid = value.(bool)
	}

	if !valid {
		return fmt.Errorf("field %q expects type %q", key, config.DataType)
	}

	if config.Regex != nil {
		if s, ok := value.(string); ok && !config.Regex.MatchString(s) {
			return fmt.Errorf("field %q does not match regex pattern", key)
		}
	}

	return nil
}

func getCountryFromHeaders(c *fiber.Ctx) string {
	if v := c.Get("x-vercel-ip-country"); v != "" {
		return v
	}
	return c.Get("cf-ipcountry")
}

func errorResponse(errorType, message string, details []string) ApiResponse {
	return ApiResponse{
		Success: false,
		Error:   &errorType,
		Message: &message,
		Details: details,
	}
}

func successResponse(message string) ApiResponse {
	return ApiResponse{Success: true, Message: &message}
}
