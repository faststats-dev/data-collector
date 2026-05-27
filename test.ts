declare const process: {
	env: Record<string, string | undefined>;
	argv: string[];
	exitCode?: number;
};

type CollectPayload = {
	server_id: string;
	data: Record<string, unknown>;
	errors?: Array<Record<string, unknown>>;
};

const endpoint = process.env.COLLECT_URL ?? "http://localhost:7000/v1/collect";
const token = process.env.PROJECT_TOKEN ?? "0f0606db75c90ca0c3681cc623a55bc8";

const defaults: CollectPayload = {
	server_id: crypto.randomUUID(),
	data: {
		player_count: 12,
		online_mode: true,
		plugin_version: "0.0.0-test",
		minecraft_version: "1.21.1",
		server_type: "paper",
		java_version: "21",
		java_vendor: "Eclipse Adoptium",
		os_name: "Linux",
		os_arch: "amd64",
		os_version: "6.8.0",
		core_count: 8,
		number_map: {
      hello: Math.floor(Math.random() * 1000),
			another: Math.floor(Math.random() * 1000),
		},
	},
};

function parseOverrides(): Partial<CollectPayload> {
	const input = process.argv.slice(2).join(" ").trim();
	if (!input) {
		return {};
	}

	try {
		return JSON.parse(input);
	} catch (error) {
		throw new Error(
			`Could not parse CLI JSON override: ${error instanceof Error ? error.message : String(error)}`,
		);
	}
}

function mergePayload(overrides: Partial<CollectPayload>): CollectPayload {
	return {
		...defaults,
		...overrides,
		data: {
			...defaults.data,
			...(overrides.data ?? {}),
		},
	};
}

async function main() {
	if (token === "replace-me-with-project-token") {
		console.warn(
			"PROJECT_TOKEN is not set; the request will probably be unauthorized.",
		);
	}

	const payload = mergePayload(parseOverrides());

	const response = await fetch(endpoint, {
		method: "POST",
		headers: {
			Authorization: `Bearer ${token}`,
			"Content-Type": "application/json",
		},
		body: JSON.stringify(payload),
	});

	const text = await response.text();
	console.log("Status:", response.status, response.statusText);
	console.log("Response:", text || "<empty>");
	console.log("Sent:", JSON.stringify(payload, null, 2));
}

main().catch((error) => {
	console.error(error);
	process.exitCode = 1;
});
