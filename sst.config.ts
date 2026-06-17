/// <reference path="./.sst/platform/config.d.ts" />

const secretNames = [
  "DATABASE_URL",
  "TINYBIRD_URL",
  "TINYBIRD_TOKEN",
  "POLAR_TOKEN",
  "REPLAY_S3_BUCKET",
  "REPLAY_S3_REGION",
  "REPLAY_S3_ENDPOINT",
  "REPLAY_S3_ACCESS_KEY_ID",
  "REPLAY_S3_SECRET_ACCESS_KEY",
  "SOURCEMAPS_S3_BUCKET",
  "SOURCEMAPS_S3_ENDPOINT",
  "SOURCEMAPS_S3_ACCESS_KEY_ID",
  "SOURCEMAPS_S3_SECRET_ACCESS_KEY",
  "SOURCEMAPS_S3_REGION",
  "SOURCEMAPS_S3_FILE_ENCRYPTION_KEY",
] as const;

const serviceEnvironment = {
  PORT: "8080",
  METRICS_PORT: "9091",
  BACKUP_DB_PATH: "/tmp/backup.db",
  RUST_LOG: "info",
  DATABASE_MAX_CONNECTIONS: "10",
  DATABASE_ACQUIRE_TIMEOUT_SECS: "5",
};

const parameterPath = "/prod/data-collector";

export default $config({
  app(input) {
    const isProduction = input.stage === "production";

    return {
      name: "data-collector",
      removal: isProduction ? "retain" : "remove",
      protect: isProduction,
      home: "aws",
    };
  },
  async run() {
    const region = aws.getRegionOutput().region;
    const accountId = aws.getCallerIdentityOutput().accountId;
    const ssm = Object.fromEntries(
      secretNames.map((name) => [
        name,
        $interpolate`arn:aws:ssm:${region}:${accountId}:parameter${parameterPath}/${name}`,
      ]),
    );

    const vpc = new sst.aws.Vpc("DataCollectorVpc");
    const cluster = new sst.aws.Cluster("DataCollectorCluster", { vpc });

    const service = new sst.aws.Service("DataCollectorService", {
      cluster,
      architecture: "arm64",
      cpu: "0.25 vCPU",
      memory: "0.5 GB",
      environment: serviceEnvironment,
      ssm,
      image: {
        context: "./",
        dockerfile: "Dockerfile",
      },
      loadBalancer: {
        rules: [
          { listen: "80/http", forward: "8080/http" },
        ],
        health: {
          "8080/http": {
            path: "/v1/health",
            interval: "30 seconds",
            timeout: "5 seconds",
            healthyThreshold: 2,
            unhealthyThreshold: 2,
          },
        },
      },
      logging: {
        retention: "1 month",
      },
    });

    return {
      url: service.url,
      ssmPath: parameterPath,
    };
  },
});
