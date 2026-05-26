import pg from "pg";

let pool: pg.Pool | null = null;

export function getDb(): pg.Pool {
  if (!pool) {
    const url = process.env.OCEAN_DB_URL;
    if (!url) {
      throw new Error("OCEAN_DB_URL not set");
    }
    pool = new pg.Pool({ connectionString: url, max: 10 });
  }
  return pool;
}
