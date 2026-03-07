// Enzyme Explore UI Types — pruned to explore-related only

export interface ChunkSummary {
  chunk_id: number;
  doc_id: number;
  chunk_index: number;
  content: string;
  chunk_word_count: number;
  title: string | null;
  file_path: string;
  created_at: number; // ms timestamp
  modified_at: number; // ms timestamp
  tags: string[];
}

export interface ChunksListResponse {
  chunks: ChunkSummary[];
}
