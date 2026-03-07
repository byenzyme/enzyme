// Enzyme API client — 7 endpoints, relative URLs (same-origin in prod, proxied in dev)

import type { ChunksListResponse } from "../types/enzyme";

const BASE = "";

export async function getChunks(): Promise<ChunksListResponse> {
  const res = await fetch(`${BASE}/chunks`);
  if (!res.ok) throw new Error(`Failed to fetch chunks: ${res.status}`);
  return res.json();
}

export async function* explore(
  query: string,
  opts?: { maxQueries?: number; resultsPerQuery?: number; signal?: AbortSignal }
): AsyncGenerator<{ event: string; data: any }> {
  const res = await fetch(`${BASE}/explore`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({
      query,
      max_queries: opts?.maxQueries ?? 4,
      results_per_query: opts?.resultsPerQuery ?? 8,
    }),
    signal: opts?.signal,
  });

  if (!res.ok) throw new Error(`Explore failed: ${res.status}`);

  const reader = res.body!.getReader();
  const decoder = new TextDecoder();
  let buffer = "";

  while (true) {
    const { done, value } = await reader.read();
    if (done) break;

    buffer += decoder.decode(value, { stream: true });
    const lines = buffer.split("\n");
    buffer = lines.pop() || "";

    let eventType = "";
    let eventData = "";

    for (const line of lines) {
      if (line.startsWith("event: ")) {
        eventType = line.slice(7).trim();
      } else if (line.startsWith("data: ")) {
        eventData = line.slice(6).trim();
      } else if (line === "" && eventType && eventData) {
        try {
          yield { event: eventType, data: JSON.parse(eventData) };
        } catch {
          // skip unparseable
        }
        eventType = "";
        eventData = "";
      }
    }
  }
}

export async function warm(query?: string): Promise<void> {
  const body: Record<string, string> = {};
  if (query) body.query = query;
  await fetch(`${BASE}/explore/warm`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  }).catch(() => {}); // fire-and-forget
}

export async function steer(
  query: string,
  signal?: AbortSignal
): Promise<Array<{ topic_name: string; bent_prompt: string }>> {
  const res = await fetch(`${BASE}/explore/steer`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ query }),
    signal,
  });
  if (!res.ok) return [];
  const data = await res.json();
  return data.suggestions ?? [];
}

export async function preview(
  query: string
): Promise<Array<{ file_path: string; score: number; content: string }>> {
  const res = await fetch(`${BASE}/explore/preview`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ query }),
  });
  if (!res.ok) return [];
  const data = await res.json();
  return data.results ?? [];
}

export async function saveSynthesisNote(
  query: string,
  markdown: string
): Promise<{ success: boolean; file_path: string; error?: string }> {
  const res = await fetch(`${BASE}/notes/save`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ query, markdown }),
  });
  return res.json();
}

export async function isHealthy(): Promise<boolean> {
  try {
    const res = await fetch(`${BASE}/health`);
    return res.ok;
  } catch {
    return false;
  }
}
