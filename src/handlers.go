package app

import (
	"bytes"
	"compress/gzip"
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

func (s *AppState) CollectHandler(c *fiber.Ctx) error {
	token := strings.TrimPrefix(c.Get("Authorization"), "Bearer ")
	if token == "" {
		return c.Status(http.StatusUnauthorized).JSON(errResp("Unauthorized", "Missing token", nil))
	}

	projectCtx, err := s.getProjectContext(c.Context(), token)
	if err != nil {
		slog.Error("Failed to get project context", "error", err)
		return c.Status(http.StatusInternalServerError).JSON(errResp("Internal Server Error", "An unexpected error occurred", nil))
	}
	if projectCtx == nil {
		return c.Status(http.StatusUnauthorized).JSON(errResp("Unauthorized", "Invalid token", nil))
	}

	var req DataRequest
	if err := decodeBody(c.Body(), &req); err != nil {
		return c.Status(http.StatusBadRequest).JSON(errResp("Validation Error", "Invalid request body", []string{err.Error()}))
	}

	data, errs := processData(c, projectCtx, req.Data)
	if len(errs) > 0 {
		return c.Status(http.StatusBadRequest).JSON(errResp("Validation Error", "Validation failed", errs))
	}
	if len(data) == 0 {
		return c.JSON(ApiResponse{Success: true, Message: ptr("No data to insert")})
	}

	dataJSON, _ := json.Marshal(data)
	if _, err := s.Pool.Exec(c.Context(),
		"INSERT INTO data_entries (project_id, server_id, data) VALUES ($1, $2, $3)",
		projectCtx.Project.ID, req.ServerID, dataJSON); err != nil {
		slog.Error("Failed to insert data", "error", err)
		return c.Status(http.StatusInternalServerError).JSON(errResp("Internal Server Error", "An unexpected error occurred", nil))
	}

	return c.JSON(ApiResponse{Success: true})
}

func decodeBody(body []byte, v any) error {
	if len(body) == 0 {
		return fmt.Errorf("empty body")
	}

	if len(body) >= 4 && body[0] == 0x28 && body[1] == 0xb5 && body[2] == 0x2f && body[3] == 0xfd {
		dec := zstdDecoderPool.Get().(*zstd.Decoder)
		defer zstdDecoderPool.Put(dec)
		if err := dec.Reset(bytes.NewReader(body)); err != nil {
			return err
		}
		return json.NewDecoder(dec).Decode(v)
	}

	if len(body) >= 2 && body[0] == 0x1f && body[1] == 0x8b {
		dec, err := gzip.NewReader(bytes.NewReader(body))
		if err != nil {
			return err
		}
		defer dec.Close()
		return json.NewDecoder(dec).Decode(v)
	}

	return json.Unmarshal(body, v)
}

func processData(c *fiber.Ctx, ctx *ProjectContext, data DataMap) (DataMap, []string) {
	filtered := make(DataMap, len(data))
	var errs []string

	if cfg, ok := ctx.ValidDataSources["country"]; ok {
		if country := c.Get("x-vercel-ip-country", c.Get("cf-ipcountry")); country != "" {
			if validateField("country", country, cfg) == nil {
				filtered["country"] = country
			}
		}
	}

	for key, val := range data {
		if slices.Contains(blockedFields, key) {
			continue
		}
		cfg, ok := ctx.ValidDataSources[key]
		if !ok {
			continue
		}
		if err := validateField(key, val, cfg); err != nil {
			errs = append(errs, err.Error())
			continue
		}
		filtered[key] = val
	}

	return filtered, errs
}

func (s *AppState) getProjectContext(ctx context.Context, token string) (*ProjectContext, error) {
	if cached, found := s.Cache.Get(token); found {
		return cached.(*ProjectContext), nil
	}

	rows, err := s.Pool.Query(ctx, `
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
			r, _ = regexp.Compile(*regexStr)
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
	s.Cache.Set(token, pCtx, cache.DefaultExpiration)
	return pCtx, nil
}

func validateField(key string, value any, cfg DataSourceConfig) error {
	var valid bool
	switch cfg.DataType {
	case "string":
		_, valid = value.(string)
	case "number":
		_, valid = value.(float64)
	case "boolean":
		_, valid = value.(bool)
	}
	if !valid {
		return fmt.Errorf("field %q expects type %q", key, cfg.DataType)
	}
	if cfg.Regex != nil {
		if s, ok := value.(string); ok && !cfg.Regex.MatchString(s) {
			return fmt.Errorf("field %q does not match regex", key)
		}
	}
	return nil
}

func errResp(errType, msg string, details []string) ApiResponse {
	return ApiResponse{Success: false, Error: &errType, Message: &msg, Details: details}
}

func ptr(s string) *string { return &s }
