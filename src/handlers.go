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
	var errors []string

	if cfg, exists := ctx.ValidDataSources["country"]; exists {
		country := c.Get("x-vercel-ip-country", c.Get("cf-ipcountry"))
		if country != "" && validateField("country", country, cfg) == nil {
			filtered["country"] = country
		}
	}

	for key, value := range data {
		if slices.Contains(blockedFields, key) {
			continue
		}

		cfg, exists := ctx.ValidDataSources[key]
		if !exists {
			continue
		}

		if err := validateField(key, value, cfg); err != nil {
			errors = append(errors, err.Error())
			continue
		}

		filtered[key] = value
	}

	return filtered, errors
}

func (s *AppState) getProjectContext(ctx context.Context, token string) (*ProjectContext, error) {
	if cached, found := s.Cache.Get(token); found {
		return cached.(*ProjectContext), nil
	}

	rows, err := s.Pool.Query(ctx, `
		SELECT p.id, d.reference_id, d.data_type, d.regex, d.allow_negative, d.allow_float, d.min_value, d.max_value
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
		var (
			pid           uuid.UUID
			refID         *string
			dataType      *string
			regexStr      *string
			allowNegative *bool
			allowFloat    *bool
			minValue      *float64
			maxValue      *float64
		)

		if err := rows.Scan(&pid, &refID, &dataType, &regexStr, &allowNegative, &allowFloat, &minValue, &maxValue); err != nil {
			return nil, err
		}

		hasRows = true
		projectID = pid

		if refID == nil || dataType == nil {
			continue
		}

		var regex *regexp.Regexp
		if regexStr != nil && *regexStr != "" {
			regex, _ = regexp.Compile(*regexStr)
		}

		dataSources[*refID] = DataSourceConfig{
			DataType:      *dataType,
			Regex:         regex,
			AllowNegative: allowNegative,
			AllowFloat:    allowFloat,
			MinValue:      minValue,
			MaxValue:      maxValue,
		}
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
	switch cfg.DataType {
	case "string":
		str, ok := value.(string)
		if !ok {
			return fmt.Errorf("field %q expects type %q", key, cfg.DataType)
		}
		if cfg.Regex != nil && !cfg.Regex.MatchString(str) {
			return fmt.Errorf("field %q does not match regex", key)
		}
		return nil

	case "number":
		num, ok := value.(float64)
		if !ok {
			return fmt.Errorf("field %q expects type %q", key, cfg.DataType)
		}
		if err := validateNumber(num, key, cfg); err != nil {
			return err
		}
		return nil

	case "boolean":
		if _, ok := value.(bool); !ok {
			return fmt.Errorf("field %q expects type %q", key, cfg.DataType)
		}
		return nil

	default:
		return fmt.Errorf("field %q expects type %q", key, cfg.DataType)
	}
}

func validateNumber(num float64, key string, cfg DataSourceConfig) error {
	if cfg.AllowNegative != nil && !*cfg.AllowNegative && num < 0 {
		return fmt.Errorf("field %q does not allow negative values", key)
	}

	if cfg.AllowFloat != nil && !*cfg.AllowFloat {
		if num != float64(int64(num)) {
			return fmt.Errorf("field %q does not allow float values", key)
		}
	}

	if cfg.MinValue != nil && num < *cfg.MinValue {
		return fmt.Errorf("field %q value must be at least %v", key, *cfg.MinValue)
	}

	if cfg.MaxValue != nil && num > *cfg.MaxValue {
		return fmt.Errorf("field %q value must be at most %v", key, *cfg.MaxValue)
	}

	return nil
}

func errResp(errType, msg string, details []string) ApiResponse {
	return ApiResponse{Success: false, Error: &errType, Message: &msg, Details: details}
}

func ptr(s string) *string { return &s }
