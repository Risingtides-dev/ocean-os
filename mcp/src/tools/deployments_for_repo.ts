import type { Tool } from "./index.js";

export const deploymentsForRepo: Tool = {
  name: "ocean.deployments_for_repo",
  description: "Recent deploy events + status for a given repo across Railway and Cloudflare.",
  inputSchema: {
    type: "object",
    properties: {
      name: { type: "string", description: "Repo name, e.g. 'risingtides-campaign-hub'" },
      since: { type: "string", description: "ISO 8601 timestamp, optional" },
    },
    required: ["name"],
  },
  handler: async (args, { db }) => {
    const since = (args.since as string) ?? new Date(Date.now() - 7 * 24 * 3600 * 1000).toISOString();
    const result = await db.query(
      `select id, type, service, status, occurred_at, payload
         from deploys.events
        where payload->>'repo' = $1
          and occurred_at >= $2
        order by occurred_at desc
        limit 50`,
      [args.name, since]
    );
    return { deployments: result.rows };
  },
};
