// Pure layout computation, extracted from worker for testability

import type { LayoutWorkerInput } from "./types";

// Deterministic pseudo-random from seed
export function seededRandom(seed: number): () => number {
  let s = seed;
  return () => {
    s = (s * 1103515245 + 12345) & 0x7fffffff;
    return s / 0x7fffffff;
  };
}

// Spiral placement for collision avoidance
export function findNonOverlappingPosition(
  targetX: number,
  targetY: number,
  width: number,
  height: number,
  placed: Array<{ x: number; y: number; width: number; height: number }>
): { x: number; y: number } {
  const padding = 20;

  const overlaps = (x: number, y: number): boolean => {
    for (const p of placed) {
      if (
        x < p.x + p.width + padding &&
        x + width + padding > p.x &&
        y < p.y + p.height + padding &&
        y + height + padding > p.y
      ) {
        return true;
      }
    }
    return false;
  };

  if (!overlaps(targetX, targetY)) {
    return { x: targetX, y: targetY };
  }

  // Spiral outward with finer increments scaled to cell size
  const startAngle = ((targetX * 7 + targetY * 13) % 100) / 100 * Math.PI * 2;
  let angle = startAngle;
  let radius = 30;
  const radiusStep = Math.max(4, (width + height) / 16);
  const maxAttempts = 500;

  for (let i = 0; i < maxAttempts; i++) {
    const x = targetX + Math.cos(angle) * radius;
    const y = targetY + Math.sin(angle) * radius;
    if (!overlaps(x, y)) {
      return { x, y };
    }
    angle += 0.4;
    radius += radiusStep;
  }

  // Fallback: just offset
  return { x: targetX + radius, y: targetY + radius };
}

export interface LayoutPosition {
  id: number;
  filePath: string;
  x: number;
  y: number;
  width: number;
  height: number;
  timelineProgress: number;
  docId: number;
  chunkIndex: number;
}

// --- Radial layout constants ---
const GOLDEN_ANGLE = Math.PI * (3 - Math.sqrt(5)); // ~2.3999... rad (~137.508 deg)
const TAG_INFLUENCE = 0.8; // 80% tag pull, 20% golden-angle dispersion

type DocChunks = LayoutWorkerInput["docs"];

/**
 * Assign each unique tag an angular "home" in [0, 2pi].
 *
 * For each doc cluster, collect its tags and record the cluster's rank in the
 * date-sorted order (oldest-first). For each tag, compute average rank across
 * all clusters containing it. Sort tags by average rank, then distribute
 * evenly around [0, 2pi].
 */
export function computeTagAngles(
  docGroups: Map<number, DocChunks>,
  sortedDocIdsOldestFirst: number[]
): Map<string, number> {
  // rank: index in oldest-first ordering
  const docRank = new Map<number, number>();
  for (let i = 0; i < sortedDocIdsOldestFirst.length; i++) {
    docRank.set(sortedDocIdsOldestFirst[i], i);
  }

  // For each tag, accumulate sum-of-ranks and count
  const tagStats = new Map<string, { sum: number; count: number }>();
  for (const docId of sortedDocIdsOldestFirst) {
    const chunks = docGroups.get(docId)!;
    const rank = docRank.get(docId)!;
    const seenTags = new Set<string>();
    for (const chunk of chunks) {
      for (const tag of chunk.tags) {
        if (!seenTags.has(tag)) {
          seenTags.add(tag);
          const stats = tagStats.get(tag) || { sum: 0, count: 0 };
          stats.sum += rank;
          stats.count += 1;
          tagStats.set(tag, stats);
        }
      }
    }
  }

  if (tagStats.size === 0) return new Map();

  // Sort tags by average rank
  const sortedTags = [...tagStats.entries()]
    .map(([tag, { sum, count }]) => ({ tag, avgRank: sum / count }))
    .sort((a, b) => a.avgRank - b.avgRank);

  // Distribute evenly around [0, 2pi]
  const tagAngles = new Map<string, number>();
  for (let i = 0; i < sortedTags.length; i++) {
    tagAngles.set(sortedTags[i].tag, (i / sortedTags.length) * 2 * Math.PI);
  }

  return tagAngles;
}

/**
 * Compute a doc cluster's target angle via circular mean of its tag angles,
 * weighted by tag frequency across all chunks.
 *
 * Returns null if the doc has no tags (caller falls back to golden angle only).
 */
export function computeDocTargetAngle(
  chunks: DocChunks,
  tagAngles: Map<string, number>
): number | null {
  // Count tag occurrences across all chunks in this doc
  const tagCounts = new Map<string, number>();
  for (const chunk of chunks) {
    for (const tag of chunk.tags) {
      if (tagAngles.has(tag)) {
        tagCounts.set(tag, (tagCounts.get(tag) || 0) + 1);
      }
    }
  }

  if (tagCounts.size === 0) return null;

  // Circular mean: average unit vectors weighted by frequency
  let sinSum = 0;
  let cosSum = 0;
  for (const [tag, count] of tagCounts) {
    const angle = tagAngles.get(tag)!;
    sinSum += Math.sin(angle) * count;
    cosSum += Math.cos(angle) * count;
  }

  return Math.atan2(sinSum, cosSum);
}

// Core layout algorithm — radial placement with tag-based angular affinity
export function computeLayout(
  docs: LayoutWorkerInput["docs"]
): LayoutPosition[] {
  if (docs.length === 0) return [];

  // Group chunks by parent doc
  const docGroups = new Map<number, typeof docs>();
  for (const chunk of docs) {
    const group = docGroups.get(chunk.docId) || [];
    group.push(chunk);
    docGroups.set(chunk.docId, group);
  }

  // Find word count range for sizing
  let maxWords = 0;
  for (const chunk of docs) {
    if (chunk.wordCount > maxWords) maxWords = chunk.wordCount;
  }
  maxWords = maxWords || 1;

  // Layout parameters
  const MIN_NODE_WIDTH = 200;
  const MAX_NODE_WIDTH = 280;
  const MIN_NODE_HEIGHT = 85;
  const MAX_NODE_HEIGHT = 100;
  const CLUSTER_OFFSET = 30;

  // baseSpacing controls Vogel spiral density (point i at radius baseSpacing * sqrt(i)).
  // Scale gently with sqrt(sqrt(N)) so large vaults spread without over-expanding.
  const avgNodeWidth = (MIN_NODE_WIDTH + MAX_NODE_WIDTH) / 2;  // 240
  const avgNodeHeight = (MIN_NODE_HEIGHT + MAX_NODE_HEIGHT) / 2; // 92.5
  const nodePadding = 40;
  const numClusters = docGroups.size;
  const baseSpacing = Math.max(
    avgNodeWidth + nodePadding,                               // minimum ~220px
    Math.sqrt((avgNodeWidth + nodePadding) * (avgNodeHeight + nodePadding) * Math.sqrt(numClusters) / Math.PI)
  );

  // Sort doc groups by creation date (oldest first) for tag angle computation
  const sortedDocIdsOldestFirst = [...docGroups.keys()].sort((a, b) => {
    const aDate = docGroups.get(a)![0].createdAt;
    const bDate = docGroups.get(b)![0].createdAt;
    return aDate - bDate;
  });

  // Newest-first order for radial placement (center = newest)
  const sortedDocIdsNewestFirst = [...sortedDocIdsOldestFirst].reverse();

  // Compute tag angular positions
  const tagAngles = computeTagAngles(docGroups, sortedDocIdsOldestFirst);

  const placed: Array<{ x: number; y: number; width: number; height: number }> =
    [];
  const positions: LayoutPosition[] = [];

  for (let docIndex = 0; docIndex < sortedDocIdsNewestFirst.length; docIndex++) {
    const docId = sortedDocIdsNewestFirst[docIndex];
    const chunks = docGroups.get(docId)!;
    // Sort chunks within doc by chunk_index
    chunks.sort((a, b) => a.chunkIndex - b.chunkIndex);

    const rng = seededRandom(docId);

    // --- Radial placement for doc cluster center ---
    // Vogel's formula: radius grows with sqrt(index), center = newest (index 0)
    const radius = baseSpacing * Math.sqrt(docIndex);
    const goldenTheta = docIndex * GOLDEN_ANGLE;
    const docTagAngle = computeDocTargetAngle(chunks, tagAngles);

    let theta: number;
    if (docTagAngle !== null) {
      // Blend tag affinity with golden angle for even dispersion
      theta = docTagAngle * TAG_INFLUENCE + goldenTheta * (1 - TAG_INFLUENCE);
    } else {
      // No tags — use golden angle with seeded jitter
      theta = goldenTheta + (rng() - 0.5) * 0.5;
    }

    const centerX = radius * Math.cos(theta);
    const centerY = radius * Math.sin(theta);

    // Place each chunk around the parent center
    for (let i = 0; i < chunks.length; i++) {
      const chunk = chunks[i];

      // Size proportional to chunk word count
      const sizeProgress = Math.sqrt(chunk.wordCount / maxWords);
      const width =
        MIN_NODE_WIDTH + (MAX_NODE_WIDTH - MIN_NODE_WIDTH) * sizeProgress;
      const height =
        MIN_NODE_HEIGHT + (MAX_NODE_HEIGHT - MIN_NODE_HEIGHT) * sizeProgress;

      // Golden-angle spiral placement for organic circular spread within cluster
      const spacing = Math.max(width, height) + CLUSTER_OFFSET;
      let targetX: number;
      let targetY: number;
      if (i === 0) {
        targetX = centerX;
        targetY = centerY;
      } else {
        const chunkAngle = i * GOLDEN_ANGLE + rng() * 0.3;
        const chunkRadius = spacing * 0.6 * Math.sqrt(i) + (rng() - 0.5) * 20;
        targetX = centerX + Math.cos(chunkAngle) * chunkRadius;
        targetY = centerY + Math.sin(chunkAngle) * chunkRadius;
      }

      const pos = findNonOverlappingPosition(
        targetX,
        targetY,
        width,
        height,
        placed
      );

      placed.push({ x: pos.x, y: pos.y, width, height });
      positions.push({
        id: chunk.id,
        filePath: chunk.filePath,
        x: pos.x,
        y: pos.y,
        width,
        height,
        timelineProgress: 0, // set below
        docId: chunk.docId,
        chunkIndex: chunk.chunkIndex,
      });
    }
  }

  // Assign timeline progress for entrance stagger
  // 0 = newest (center, appears first) → 1 = oldest (periphery, appears last)
  for (let i = 0; i < positions.length; i++) {
    positions[i].timelineProgress = i / Math.max(1, positions.length - 1);
  }

  return positions;
}
