import type { Tool } from "./index.js";

export const searchKnowledge: Tool = {
  name: "ocean.search_knowledge",
  description: "Generic semantic search across all ingested text — Slack, GitHub, campaigns, content, deploys.",
  inputSchema: {
    type: "object",
    properties: {
      query: { type: "string" },
      limit: { type: "number", default: 10 },
      source: { type: "string", description: "Filter to one source, optional" },
    },
    required: ["query"],
  },
  handler: async (args) => {
    // TODO: embed query, ANN search over knowledge.embeddings
    return {
      todo: "not implemented",
      received: args,
    };
  },
};
