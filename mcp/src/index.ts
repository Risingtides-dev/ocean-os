/**
 * Ocean-OS MCP server — skeleton.
 *
 * This is the single MCP every Slack bot loads. Tools here hide source-specific
 * complexity behind clean verbs that map to operator intent.
 *
 * The tools below are stubs — they validate args and return shape-correct
 * placeholder data. Replace each tool's implementation with a real Postgres
 * query as ingestion workers come online.
 */

import { Server } from "@modelcontextprotocol/sdk/server/index.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import {
  CallToolRequestSchema,
  ListToolsRequestSchema,
} from "@modelcontextprotocol/sdk/types.js";
import { Pool } from "pg";
import { z } from "zod";

const pool = new Pool({
  connectionString: process.env.OCEAN_DATABASE_URL,
  // Read-only role recommended.
});

// ----------------------------------------------------------------------------
// Tool definitions
// ----------------------------------------------------------------------------

const tools = [
  {
    name: "ocean.query_campaign",
    description:
      "Return full state for a campaign — bookings, creators, posts, performance.",
    inputSchema: {
      type: "object",
      properties: {
        slug: { type: "string", description: "Campaign slug from Campaign Hub." },
      },
      required: ["slug"],
    },
  },
  {
    name: "ocean.search_threads",
    description:
      "Semantic search across Slack history. Returns thread summaries ranked by relevance.",
    inputSchema: {
      type: "object",
      properties: {
        query: { type: "string" },
        limit: { type: "number", default: 10 },
      },
      required: ["query"],
    },
  },
  {
    name: "ocean.deployments_for_repo",
    description: "Recent deploys for a repo, with status and related commits.",
    inputSchema: {
      type: "object",
      properties: {
        repo: { type: "string", description: "owner/name" },
        since_hours: { type: "number", default: 24 },
      },
      required: ["repo"],
    },
  },
  {
    name: "ocean.creator_history",
    description:
      "Every campaign a creator has worked on, with performance summary.",
    inputSchema: {
      type: "object",
      properties: {
        handle: { type: "string", description: "Platform handle, e.g. @username" },
      },
      required: ["handle"],
    },
  },
  {
    name: "ocean.find_top_posts",
    description:
      "Top-performing posts for an artist/campaign, optionally with embeddings for downstream caption generation.",
    inputSchema: {
      type: "object",
      properties: {
        artist: { type: "string" },
        limit: { type: "number", default: 10 },
        include_embeddings: { type: "boolean", default: false },
      },
      required: ["artist"],
    },
  },
  {
    name: "ocean.diagnose_deploy",
    description:
      "Pull related commits, Railway logs, and Cloudflare state for a deploy event. Use for 'my deploy is broken'.",
    inputSchema: {
      type: "object",
      properties: {
        repo: { type: "string" },
        sha: { type: "string", description: "Optional commit SHA to focus on" },
      },
      required: ["repo"],
    },
  },
  {
    name: "ocean.post_content_to_telegram",
    description:
      "Generate content via Content Posting Lab and distribute to a Telegram folder. Wraps the Lab pipeline as one tool.",
    inputSchema: {
      type: "object",
      properties: {
        folder: { type: "string", description: "Telegram folder name or id" },
        brief: { type: "string", description: "Plain-language content brief" },
      },
      required: ["folder", "brief"],
    },
  },
  {
    name: "ocean.log_agent_action",
    description:
      "Write to the agent feedback log. Every meaningful action should call this.",
    inputSchema: {
      type: "object",
      properties: {
        agent_id: { type: "string" },
        tool_name: { type: "string" },
        args: { type: "object" },
        result: { type: "object" },
      },
      required: ["agent_id", "tool_name"],
    },
  },
];

// ----------------------------------------------------------------------------
// Stub implementations — replace as ingestion workers come online
// ----------------------------------------------------------------------------

async function handleQueryCampaign(args: { slug: string }) {
  const { slug } = z.object({ slug: z.string() }).parse(args);
  // TODO: real query joining campaigns.bookings + cobrand.posts + cobrand.campaigns
  return {
    slug,
    status: "stub",
    note:
      "Ocean MCP not yet wired to live data. Implement once Campaign Hub + Cobrand ingestion workers are deployed.",
  };
}

async function handleSearchThreads(args: { query: string; limit?: number }) {
  const { query, limit = 10 } = z
    .object({ query: z.string(), limit: z.number().optional() })
    .parse(args);
  // TODO: pgvector ANN query against embeddings(entity_type='slack_thread')
  return { query, limit, results: [], note: "Slack ingestion not yet deployed." };
}

async function handleDeploymentsForRepo(args: {
  repo: string;
  since_hours?: number;
}) {
  const { repo, since_hours = 24 } = z
    .object({ repo: z.string(), since_hours: z.number().optional() })
    .parse(args);

  try {
    const { rows } = await pool.query(
      `SELECT id, environment, status, sha, started_at, finished_at
       FROM github.deploys
       WHERE repo = $1 AND started_at > now() - ($2 || ' hours')::interval
       ORDER BY started_at DESC`,
      [repo, since_hours]
    );
    return { repo, deploys: rows };
  } catch (err) {
    return {
      repo,
      deploys: [],
      note: `Database unavailable or schema not initialized: ${(err as Error).message}`,
    };
  }
}

async function handleCreatorHistory(args: { handle: string }) {
  const { handle } = z.object({ handle: z.string() }).parse(args);
  return { handle, campaigns: [], note: "Campaign Hub ingestion not yet deployed." };
}

async function handleFindTopPosts(args: {
  artist: string;
  limit?: number;
  include_embeddings?: boolean;
}) {
  const { artist, limit = 10 } = z
    .object({
      artist: z.string(),
      limit: z.number().optional(),
      include_embeddings: z.boolean().optional(),
    })
    .parse(args);
  return { artist, limit, posts: [], note: "Cobrand ingestion not yet deployed." };
}

async function handleDiagnoseDeploy(args: { repo: string; sha?: string }) {
  return {
    repo: args.repo,
    sha: args.sha,
    diagnosis: "stub",
    note: "Wire Railway + Cloudflare ingestion before this is meaningful.",
  };
}

async function handlePostContentToTelegram(args: {
  folder: string;
  brief: string;
}) {
  return {
    folder: args.folder,
    brief: args.brief,
    status: "stub",
    note:
      "Will call Content Posting Lab API once that integration is wired. For now, this is a placeholder.",
  };
}

async function handleLogAgentAction(args: {
  agent_id: string;
  tool_name: string;
  args?: unknown;
  result?: unknown;
}) {
  try {
    const { rows } = await pool.query(
      `INSERT INTO agent.events (agent_id, tool_name, args, result)
       VALUES ($1, $2, $3, $4)
       RETURNING id`,
      [args.agent_id, args.tool_name, args.args ?? null, args.result ?? null]
    );
    return { logged: true, event_id: rows[0]?.id };
  } catch (err) {
    return { logged: false, error: (err as Error).message };
  }
}

// ----------------------------------------------------------------------------
// Server wiring
// ----------------------------------------------------------------------------

const server = new Server(
  { name: "ocean-os", version: "0.0.1" },
  { capabilities: { tools: {} } }
);

server.setRequestHandler(ListToolsRequestSchema, async () => ({ tools }));

server.setRequestHandler(CallToolRequestSchema, async (req) => {
  const { name, arguments: args } = req.params;
  const a = args as Record<string, unknown>;
  let payload: unknown;
  switch (name) {
    case "ocean.query_campaign":
      payload = await handleQueryCampaign(a as { slug: string });
      break;
    case "ocean.search_threads":
      payload = await handleSearchThreads(a as { query: string; limit?: number });
      break;
    case "ocean.deployments_for_repo":
      payload = await handleDeploymentsForRepo(
        a as { repo: string; since_hours?: number }
      );
      break;
    case "ocean.creator_history":
      payload = await handleCreatorHistory(a as { handle: string });
      break;
    case "ocean.find_top_posts":
      payload = await handleFindTopPosts(
        a as { artist: string; limit?: number; include_embeddings?: boolean }
      );
      break;
    case "ocean.diagnose_deploy":
      payload = await handleDiagnoseDeploy(a as { repo: string; sha?: string });
      break;
    case "ocean.post_content_to_telegram":
      payload = await handlePostContentToTelegram(
        a as { folder: string; brief: string }
      );
      break;
    case "ocean.log_agent_action":
      payload = await handleLogAgentAction(
        a as {
          agent_id: string;
          tool_name: string;
          args?: unknown;
          result?: unknown;
        }
      );
      break;
    default:
      throw new Error(`Unknown tool: ${name}`);
  }
  return {
    content: [{ type: "text", text: JSON.stringify(payload, null, 2) }],
  };
});

const transport = new StdioServerTransport();
await server.connect(transport);
