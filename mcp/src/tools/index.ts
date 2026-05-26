import type { Pool } from "pg";
import { queryCampaign } from "./query_campaign.js";
import { searchThreads } from "./search_threads.js";
import { deploymentsForRepo } from "./deployments_for_repo.js";
import { searchKnowledge } from "./search_knowledge.js";

export type ToolContext = { db: Pool };

export type Tool = {
  name: string;
  description: string;
  inputSchema: Record<string, unknown>;
  handler: (args: Record<string, unknown>, ctx: ToolContext) => Promise<unknown>;
};

export const tools: Tool[] = [
  queryCampaign,
  searchThreads,
  deploymentsForRepo,
  searchKnowledge,
];
