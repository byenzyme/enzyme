// NodeRenderer: draws chunk nodes at various LOD levels (ported from aficionado)

import type { ChunkNode } from "./types";
import {
  COLORS,
  SIMPLE_NODE_ZOOM_THRESHOLD,
  TEXT_RENDER_ZOOM_THRESHOLD,
  pathHue,
} from "./types";

// Text measurement cache
const textWidthCache = new Map<string, number>();
const MAX_CACHE_SIZE = 5000;

function measureText(ctx: CanvasRenderingContext2D, text: string, font: string): number {
  const key = `${font}|${text}`;
  let w = textWidthCache.get(key);
  if (w !== undefined) return w;

  ctx.font = font;
  w = ctx.measureText(text).width;

  if (textWidthCache.size > MAX_CACHE_SIZE) {
    textWidthCache.clear();
  }
  textWidthCache.set(key, w);
  return w;
}

function truncateText(
  ctx: CanvasRenderingContext2D,
  text: string,
  maxWidth: number,
  font: string
): string {
  if (measureText(ctx, text, font) <= maxWidth) return text;
  let lo = 0;
  let hi = text.length;
  while (lo < hi) {
    const mid = (lo + hi + 1) >> 1;
    if (measureText(ctx, text.slice(0, mid) + "\u2026", font) <= maxWidth) {
      lo = mid;
    } else {
      hi = mid - 1;
    }
  }
  return text.slice(0, lo) + "\u2026";
}

// Break a single long word (e.g. URL) into chunks that fit within maxWidth,
// appending a hyphen at the end of each broken segment.
function breakLongWord(
  ctx: CanvasRenderingContext2D,
  word: string,
  maxWidth: number,
  font: string
): string[] {
  const parts: string[] = [];
  let remaining = word;
  while (remaining) {
    if (measureText(ctx, remaining, font) <= maxWidth) {
      parts.push(remaining);
      break;
    }
    // Binary search for how many chars fit with a trailing hyphen
    let lo = 1;
    let hi = remaining.length;
    while (lo < hi) {
      const mid = (lo + hi + 1) >> 1;
      if (measureText(ctx, remaining.slice(0, mid) + "-", font) <= maxWidth) {
        lo = mid;
      } else {
        hi = mid - 1;
      }
    }
    if (lo <= 0) lo = 1; // always consume at least 1 char
    parts.push(remaining.slice(0, lo) + "-");
    remaining = remaining.slice(lo);
  }
  return parts;
}

function wrapText(
  ctx: CanvasRenderingContext2D,
  text: string,
  maxWidth: number,
  font: string,
  maxLines: number
): string[] {
  const words = text.split(/\s+/);
  const lines: string[] = [];
  let currentLine = "";

  for (const word of words) {
    const testLine = currentLine ? `${currentLine} ${word}` : word;
    if (measureText(ctx, testLine, font) <= maxWidth) {
      currentLine = testLine;
    } else {
      if (currentLine) {
        lines.push(currentLine);
        if (lines.length >= maxLines) {
          lines[lines.length - 1] = truncateText(ctx, lines[lines.length - 1], maxWidth, font);
          return lines;
        }
      }
      // If the word itself is too wide, break it with hyphens
      if (measureText(ctx, word, font) > maxWidth) {
        const parts = breakLongWord(ctx, word, maxWidth, font);
        for (const part of parts) {
          if (lines.length >= maxLines) {
            lines[lines.length - 1] = truncateText(ctx, lines[lines.length - 1], maxWidth, font);
            return lines;
          }
          // Last part (no trailing hyphen) becomes the new currentLine
          if (part === parts[parts.length - 1]) {
            currentLine = part;
          } else {
            lines.push(part);
          }
        }
      } else {
        currentLine = word;
      }
    }
  }
  if (currentLine) {
    if (lines.length >= maxLines) {
      lines[lines.length - 1] = truncateText(
        ctx,
        lines[lines.length - 1] + " " + currentLine,
        maxWidth,
        font
      );
    } else {
      lines.push(currentLine);
    }
  }
  return lines;
}

function roundedRect(
  ctx: CanvasRenderingContext2D,
  x: number,
  y: number,
  w: number,
  h: number,
  r: number
): void {
  const cr = Math.min(r, w / 2, h / 2);
  ctx.beginPath();
  ctx.moveTo(x + cr, y);
  ctx.lineTo(x + w - cr, y);
  ctx.quadraticCurveTo(x + w, y, x + w, y + cr);
  ctx.lineTo(x + w, y + h - cr);
  ctx.quadraticCurveTo(x + w, y + h, x + w - cr, y + h);
  ctx.lineTo(x + cr, y + h);
  ctx.quadraticCurveTo(x, y + h, x, y + h - cr);
  ctx.lineTo(x, y + cr);
  ctx.quadraticCurveTo(x, y, x + cr, y);
  ctx.closePath();
}

export class NodeRenderer {
  // Draw a simple rectangle (zoomed out)
  drawSimple(ctx: CanvasRenderingContext2D, node: ChunkNode): void {
    const { x, y, width, height } = node;
    const hue = pathHue(node.filePath);

    roundedRect(ctx, x, y, width, height, 4);

    // Background — per-cluster hue with visible saturation
    // At overview zoom each node can be only a few screen pixels wide, so it
    // needs substantially more luminance than a close-up card to stay legible.
    ctx.fillStyle = `hsl(${hue}, 42%, 36%)`;
    ctx.fill();

    // Border — per-cluster bright hue for highlights
    if (node.hasSearchResults) {
      ctx.strokeStyle = COLORS.searchBorder;
      ctx.lineWidth = 2;
    } else if (node.isRelevant) {
      ctx.strokeStyle = `hsl(${hue}, 75%, 60%)`;
      ctx.lineWidth = 2;
    } else {
      ctx.strokeStyle = COLORS.nodeBorder;
      ctx.lineWidth = 1;
    }
    ctx.stroke();
  }

  // Draw full node with text content
  draw(ctx: CanvasRenderingContext2D, node: ChunkNode, zoom: number): void {
    const { x, y, width, height } = node;
    const padding = 12;
    const contentWidth = width - padding * 2;

    // Background
    roundedRect(ctx, x, y, width, height, 8);

    // Per-cluster hue with visible saturation
    const hue = pathHue(node.filePath);
    ctx.fillStyle = `hsl(${hue}, 20%, 17%)`;
    ctx.fill();

    // Border — per-cluster bright hue for highlights
    if (node.hasSearchResults) {
      ctx.strokeStyle = COLORS.searchBorder;
      ctx.lineWidth = 2;
    } else if (node.isRelevant) {
      ctx.strokeStyle = `hsl(${hue}, 75%, 60%)`;
      ctx.lineWidth = 2;
    } else {
      ctx.strokeStyle = COLORS.nodeBorder;
      ctx.lineWidth = 1;
    }
    ctx.stroke();

    if (zoom < TEXT_RENDER_ZOOM_THRESHOLD) return;

    const metaFont = "11px system-ui, -apple-system, sans-serif";
    const excerptFont = "12px system-ui, -apple-system, sans-serif";

    // Excerpt (primary content - truncated chunk text)
    const excerpt = node.content
      ? node.content.replace(/^#+ .+\n?/, "").replace(/\s+/g, " ").trim()
      : "";
    if (excerpt) {
      ctx.fillStyle = COLORS.titleText;
      const maxExcerptLines = height > 90 ? 3 : 2;
      const lines = wrapText(ctx, excerpt, contentWidth, excerptFont, maxExcerptLines);
      for (let i = 0; i < lines.length; i++) {
        ctx.font = excerptFont;
        ctx.fillText(lines[i], x + padding, y + padding + 13 + i * 16);
      }

      // Gradient fade on leading edge when text starts mid-word
      if (/^[a-z,;:'\-]/.test(excerpt)) {
        const fadeW = 28;
        const fadeGrad = ctx.createLinearGradient(x + padding, 0, x + padding + fadeW, 0);
        fadeGrad.addColorStop(0, `hsla(${hue}, 20%, 17%, 1)`);
        fadeGrad.addColorStop(1, `hsla(${hue}, 20%, 17%, 0)`);
        ctx.fillStyle = fadeGrad;
        ctx.fillRect(x + padding, y + padding, fadeW, 16);
      }

      // File path (dimmed, below excerpt)
      const pathY = y + padding + 13 + lines.length * 16 + 4;
      ctx.fillStyle = COLORS.nodeTextDim;
      ctx.font = metaFont;
      const shortPath = truncateText(ctx, node.filePath, contentWidth, metaFont);
      ctx.fillText(shortPath, x + padding, pathY);
    } else {
      // Fallback: show title if no content
      const titleFont = "bold 13px system-ui, -apple-system, sans-serif";
      const title = node.title || node.filePath.split("/").pop() || "Untitled";
      ctx.fillStyle = COLORS.titleText;
      ctx.font = titleFont;
      ctx.fillText(truncateText(ctx, title, contentWidth, titleFont), x + padding, y + padding + 13);

      ctx.fillStyle = COLORS.nodeTextDim;
      ctx.font = metaFont;
      const shortPath = truncateText(ctx, node.filePath, contentWidth, metaFont);
      ctx.fillText(shortPath, x + padding, y + padding + 28);
    }

    // Search score badge
    if (node.hasSearchResults && node.searchScore !== undefined) {
      const scoreStr = `${Math.round(node.searchScore * 100)}%`;
      const badgeFont = "bold 10px system-ui, -apple-system, sans-serif";
      ctx.font = badgeFont;
      const sw = measureText(ctx, scoreStr, badgeFont) + 10;
      const bx = x + width - sw - 8;
      const by = y + 8;

      roundedRect(ctx, bx, by, sw, 18, 4);
      ctx.fillStyle = "rgba(80, 160, 255, 0.3)";
      ctx.fill();
      ctx.fillStyle = COLORS.searchBorder;
      ctx.fillText(scoreStr, bx + 5, by + 13);
    }
  }

  // Draw expanded hover card
  drawExpanded(
    ctx: CanvasRenderingContext2D,
    node: ChunkNode,
    progress: number // 0-1 animation
  ): void {
    const expandedWidth = 320;
    const hasExcerpt = node.content || node.searchContent;
    const expandedHeight = hasExcerpt ? 220 : 140;

    // Interpolate dimensions
    const w = node.width + (expandedWidth - node.width) * progress;
    const h = node.height + (expandedHeight - node.height) * progress;
    const cx = node.x + node.width / 2;
    const cy = node.y + node.height / 2;
    const x = cx - w / 2;
    const y = cy - h / 2;
    const padding = 14;
    const contentWidth = w - padding * 2;

    // Shadow
    ctx.save();
    ctx.shadowColor = "rgba(0,0,0,0.5)";
    ctx.shadowBlur = 20 * progress;
    ctx.shadowOffsetY = 4 * progress;

    const hue = pathHue(node.filePath);
    roundedRect(ctx, x, y, w, h, 10);
    ctx.fillStyle = `hsl(${hue}, 22%, 19%)`;
    ctx.fill();
    ctx.restore();

    // Border — per-cluster bright hue for highlights
    if (node.hasSearchResults) {
      ctx.strokeStyle = COLORS.searchBorder;
      ctx.lineWidth = 2;
    } else if (node.isRelevant) {
      ctx.strokeStyle = `hsl(${hue}, 75%, 60%)`;
      ctx.lineWidth = 2;
    } else {
      ctx.strokeStyle = COLORS.nodeBorder;
      ctx.lineWidth = 1.5;
    }
    roundedRect(ctx, x, y, w, h, 10);
    ctx.stroke();

    if (progress < 0.1) return;
    const textAlpha = Math.min(1, (progress - 0.1) / 0.3);
    ctx.globalAlpha = textAlpha;

    // Title (bold, prominent on hover)
    const titleFont = "bold 14px system-ui, -apple-system, sans-serif";
    const title = node.title || node.filePath.split("/").pop() || "Untitled";
    ctx.fillStyle = COLORS.titleText;
    ctx.font = titleFont;
    ctx.fillText(truncateText(ctx, title, contentWidth, titleFont), x + padding, y + padding + 14);

    // File path
    const metaFont = "11px system-ui, -apple-system, sans-serif";
    ctx.fillStyle = COLORS.nodeTextDim;
    ctx.font = metaFont;
    ctx.fillText(truncateText(ctx, node.filePath, contentWidth, metaFont), x + padding, y + padding + 30);

    // Date + word count
    const date = new Date(node.createdAt);
    const dateStr = date.toLocaleDateString("en-US", {
      month: "short",
      day: "numeric",
      year: "numeric",
    });
    ctx.fillText(
      `${dateStr} \u00b7 ${node.wordCount.toLocaleString()} words`,
      x + padding,
      y + padding + 44
    );

    // Excerpt (untruncated, below divider)
    if (progress > 0.3 && hasExcerpt) {
      const excerptAlpha = Math.min(1, (progress - 0.3) / 0.3);
      ctx.globalAlpha = excerptAlpha;

      // Divider
      ctx.strokeStyle = COLORS.nodeBorder;
      ctx.lineWidth = 0.5;
      ctx.beginPath();
      ctx.moveTo(x + padding, y + padding + 56);
      ctx.lineTo(x + w - padding, y + padding + 56);
      ctx.stroke();

      // Use searchContent if available (more relevant), otherwise chunk content
      const excerptSource = node.searchContent
        || node.content.replace(/^#+ .+\n?/, "").replace(/\s+/g, " ").trim();
      const excerptFont = "12px system-ui, -apple-system, sans-serif";
      ctx.fillStyle = COLORS.nodeText;
      const lines = wrapText(ctx, excerptSource, contentWidth, excerptFont, 7);
      for (let i = 0; i < lines.length; i++) {
        ctx.font = excerptFont;
        ctx.fillText(lines[i], x + padding, y + padding + 72 + i * 17);
      }
    }

    ctx.globalAlpha = 1;
  }

  // Draw source-expanded card (medium size, between compact and hover)
  drawSourceExpanded(
    ctx: CanvasRenderingContext2D,
    node: ChunkNode,
    progress: number // 0-1 animation
  ): void {
    const expandedWidth = 240;
    const expandedHeight = 160;

    // Interpolate dimensions from node's original size
    const w = node.width + (expandedWidth - node.width) * progress;
    const h = node.height + (expandedHeight - node.height) * progress;
    const cx = node.x + node.width / 2;
    const cy = node.y + node.height / 2;
    const x = cx - w / 2;
    const y = cy - h / 2;
    const padding = 12;
    const contentWidth = w - padding * 2;

    const hue = pathHue(node.filePath);

    // Background (no shadow — keeps it lighter for multiple simultaneous expansions)
    roundedRect(ctx, x, y, w, h, 8);
    ctx.fillStyle = `hsl(${hue}, 20%, 17%)`;
    ctx.fill();

    // Higher saturation border to visually link nodes from same source
    ctx.strokeStyle = `hsl(${hue}, 50%, 40%)`;
    ctx.lineWidth = 2;
    roundedRect(ctx, x, y, w, h, 8);
    ctx.stroke();

    if (progress < 0.1) return;
    const textAlpha = Math.min(1, (progress - 0.1) / 0.3);
    ctx.globalAlpha = textAlpha;

    const titleFont = "bold 13px system-ui, -apple-system, sans-serif";
    const excerptFont = "13px system-ui, -apple-system, sans-serif";
    const metaFont = "11px system-ui, -apple-system, sans-serif";

    // Title
    const title = node.title || node.filePath.split("/").pop() || "Untitled";
    ctx.fillStyle = COLORS.titleText;
    ctx.font = titleFont;
    ctx.fillText(truncateText(ctx, title, contentWidth, titleFont), x + padding, y + padding + 13);

    // Excerpt (5-line max, 13px font)
    const excerpt = node.content
      ? node.content.replace(/^#+ .+\n?/, "").replace(/\s+/g, " ").trim()
      : "";
    if (excerpt && progress > 0.2) {
      const excerptAlpha = Math.min(1, (progress - 0.2) / 0.3);
      ctx.globalAlpha = excerptAlpha;
      ctx.fillStyle = COLORS.nodeText;
      const lines = wrapText(ctx, excerpt, contentWidth, excerptFont, 5);
      for (let i = 0; i < lines.length; i++) {
        ctx.font = excerptFont;
        ctx.fillText(lines[i], x + padding, y + padding + 30 + i * 17);
      }

      // File path (dimmed, at bottom)
      const pathY = y + h - padding;
      ctx.fillStyle = COLORS.nodeTextDim;
      ctx.font = metaFont;
      ctx.fillText(truncateText(ctx, node.filePath, contentWidth, metaFont), x + padding, pathY);
    }

    ctx.globalAlpha = 1;
  }

  // Draw with entrance animation (position + opacity override)
  drawAnimated(
    ctx: CanvasRenderingContext2D,
    node: ChunkNode,
    animX: number,
    animY: number,
    animOpacity: number,
    zoom: number
  ): void {
    const originalX = node.x;
    const originalY = node.y;
    node.x = animX;
    node.y = animY;

    ctx.globalAlpha = animOpacity;

    if (zoom < SIMPLE_NODE_ZOOM_THRESHOLD) {
      this.drawSimple(ctx, node);
    } else {
      this.draw(ctx, node, zoom);
    }

    ctx.globalAlpha = 1;
    node.x = originalX;
    node.y = originalY;
  }
}
