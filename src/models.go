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
	DataType string         `json:"data_type"`
	Regex    *regexp.Regexp `json:"-"`
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
