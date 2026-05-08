import type { Tool } from "./index.js";

export const queryCampaign: Tool = {
  name: "ocean.query_campaign",
  description: "Return the full state of one campaign — client, budget, creators, posts, performance.",
  inputSchema: {
    type: "object",
    properties: {
      slug: { type: "string", description: "Campaign slug, e.g. 'warner-may-26'" },
    },
    required: ["slug"],
  },
  handler: async (args) => {
    // TODO: wire up to campaigns.events + materialized views
    return {
      todo: "not implemented",
      received: args,
    };
  },
};
