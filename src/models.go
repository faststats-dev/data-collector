package app

import (
	"regexp"

	"github.com/google/uuid"
)

type ProjectContext struct {
	Project          Project
	ValidDataSources map[string]DataSourceConfig
}

type Project struct {
	ID uuid.UUID `db:"id"`
}

type DataSourceConfig struct {
	DataType      string         `json:"data_type"`
	Regex         *regexp.Regexp `json:"-"`
	AllowNegative *bool          `json:"allow_negative,omitempty"`
	AllowFloat    *bool          `json:"allow_float,omitempty"`
	MinValue      *float64       `json:"min_value,omitempty"`
	MaxValue      *float64       `json:"max_value,omitempty"`
}

type DataMap = map[string]any

type DataRequest struct {
	ServerID uuid.UUID `json:"server_id"`
	Data     DataMap   `json:"data"`
}

type ApiResponse struct {
	Success bool     `json:"success"`
	Error   *string  `json:"error,omitempty"`
	Message *string  `json:"message,omitempty"`
	Details []string `json:"details,omitempty"`
}
