import type { Tool } from "./index.js";

export const searchThreads: Tool = {
  name: "ocean.search_threads",
  description: "Semantic search across Slack threads. Optionally scoped to a channel.",
  inputSchema: {
    type: "object",
    properties: {
      query: { type: "string" },
      channel: { type: "string", description: "Slack channel id, optional" },
      limit: { type: "number", default: 10 },
    },
    required: ["query"],
  },
  handler: async (args) => {
    // TODO: embed query, search knowledge.embeddings where source='slack'
    return {
      todo: "not implemented",
      received: args,
    };
  },
};
