package app

import (
	"log/slog"
	"net/http"
	"os"

	"github.com/gofiber/fiber/v2"
	"github.com/gofiber/fiber/v2/middleware/logger"
	"github.com/gofiber/fiber/v2/middleware/recover"
	"github.com/joho/godotenv"
)

func Run() {
	_ = godotenv.Load()

	loggerInstance := slog.New(slog.NewTextHandler(os.Stdout, nil))
	slog.SetDefault(loggerInstance)

	dbUrl := os.Getenv("DATABASE_URL")
	if dbUrl == "" {
		slog.Error("DATABASE_URL must be set in .env file or environment variables")
		os.Exit(1)
	}

	state, err := NewAppState(dbUrl)
	if err != nil {
		slog.Error("Failed to connect to database", "error", err)
		os.Exit(1)
	}
	defer state.Pool.Close()

	app := fiber.New(fiber.Config{
		DisableStartupMessage: true,
	})

	app.Use(logger.New())
	app.Use(recover.New())

	app.Post("/v1/collect", state.CollectHandler)
	app.Get("/v1/health", func(c *fiber.Ctx) error {
		return c.SendStatus(http.StatusOK)
	})

	port := os.Getenv("PORT")
	if port == "" {
		port = "5000"
	}

	addr := "0.0.0.0:" + port
	slog.Info("Server running", "address", "http://"+addr)

	if err := app.Listen(addr); err != nil {
		slog.Error("Server failed to start", "error", err)
		os.Exit(1)
	}
}
