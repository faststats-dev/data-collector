package app

import (
	"context"
	"time"

	"github.com/jackc/pgx/v5/pgxpool"
	"github.com/patrickmn/go-cache"
)

type AppState struct {
	Pool  *pgxpool.Pool
	Cache *cache.Cache
}

func NewAppState(dbUrl string) (*AppState, error) {
	config, err := pgxpool.ParseConfig(dbUrl)
	if err != nil {
		return nil, err
	}

	config.MaxConns = 50

	pool, err := pgxpool.NewWithConfig(context.Background(), config)
	if err != nil {
		return nil, err
	}

	c := cache.New(5*time.Minute, 10*time.Minute)

	return &AppState{
		Pool:  pool,
		Cache: c,
	}, nil
}
