import { defineConfig } from "orval";

// Generates a typed React Query client from the host's checked-in OpenAPI document.
// Regenerate after any management-API change: `bun run api:gen` (the Rust side writes
// api/openapi.json with `cargo run -p punktfunk-host -- openapi > api/openapi.json`).
export default defineConfig({
	punktfunk: {
		input: {
			target: "../api/openapi.json",
		},
		output: {
			mode: "tags-split",
			target: "./src/api/gen",
			schemas: "./src/api/gen/model",
			client: "react-query",
			clean: true,
			override: {
				mutator: {
					path: "./src/api/fetcher.ts",
					name: "apiFetch",
				},
				// The mutator returns the response BODY (it throws on HTTP errors), not a
				// `{status,data,headers}` envelope — so a query's `.data` is the typed payload.
				fetch: {
					includeHttpResponseReturnType: false,
				},
				// No global query/mutation override: orval picks `useQuery` for GET and
				// `useMutation` for POST/DELETE by HTTP method, which is what the pages expect.
			},
		},
	},
});
