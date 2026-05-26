/**
 * Cobrand HTTP client.
 *
 * Fetches campaign performance data from the Cobrand API.
 * Handles 429 rate limiting with exponential backoff.
 *
 * NOTE: The exact endpoint paths and response envelope here are based on
 * reasonable conventions. Verify against campaign_manager/services/cobrand.py
 * in the campaign-hub repo and adjust paths/field names if needed.
 */

export interface CobrandCampaign {
  id: string;
  slug: string;
  client: string | null;
  artist: string | null;
  status: string;
  total_views: number;
  total_posts: number;
}

export interface CobrandPost {
  id: string;
  campaign_id: string;
  creator_handle: string;
  url: string;
  views: number;
  engagement: number;
  posted_at: string | null;
}

export class CobrandClient {
  private readonly base: string;
  private readonly apiKey: string;

  constructor(base: string, apiKey: string) {
    this.base = base.replace(/\/$/, "");
    this.apiKey = apiKey;
  }

  /**
   * Fetch campaign summary by slug. Returns null if the campaign isn't found.
   * Endpoint: GET /campaigns/{slug}
   */
  async fetchCampaign(slug: string): Promise<CobrandCampaign | null> {
    return this.get<CobrandCampaign>(`/campaigns/${encodeURIComponent(slug)}`);
  }

  /**
   * Fetch all posts for a campaign.
   * Endpoint: GET /campaigns/{id}/posts
   * Response envelope: { posts: CobrandPost[] } or CobrandPost[] directly.
   */
  async fetchCampaignPosts(campaignId: string): Promise<CobrandPost[]> {
    const result = await this.get<CobrandPost[] | { posts: CobrandPost[] }>(
      `/campaigns/${encodeURIComponent(campaignId)}/posts`
    );
    if (!result) return [];
    return Array.isArray(result) ? result : (result.posts ?? []);
  }

  private async get<T>(path: string, attempt = 0): Promise<T | null> {
    const url = `${this.base}${path}`;
    let res: Response;

    try {
      res = await fetch(url, {
        headers: {
          Authorization: `Bearer ${this.apiKey}`,
          Accept: "application/json",
        },
        signal: AbortSignal.timeout(30_000),
      });
    } catch (err) {
      throw new Error(`Network error fetching ${url}: ${err}`);
    }

    if (res.status === 429) {
      if (attempt >= 4) {
        throw new Error(`Rate limited on ${url} — giving up after ${attempt} retries`);
      }
      const retryAfterSec = Number(res.headers.get("retry-after") ?? 0);
      const waitMs = retryAfterSec > 0
        ? retryAfterSec * 1000
        : Math.pow(2, attempt + 1) * 1000; // 2s, 4s, 8s, 16s
      await sleep(waitMs);
      return this.get<T>(path, attempt + 1);
    }

    if (res.status === 404) return null;

    if (!res.ok) {
      throw new Error(`HTTP ${res.status} fetching ${url}: ${await res.text().catch(() => "")}`);
    }

    return (await res.json()) as T;
  }
}

export function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}
