// Quadtree: spatial indexing for viewport culling & hit detection (ported from aficionado)

import type { Bounds } from "./Camera";

export interface QuadtreeNode {
  id: string;
  x: number;
  y: number;
  width: number;
  height: number;
}

const MAX_OBJECTS = 10;
const MAX_LEVELS = 8;

export class Quadtree<T extends QuadtreeNode> {
  private bounds: Bounds;
  private level: number;
  private objects: T[] = [];
  private children: Quadtree<T>[] | null = null;

  constructor(bounds: Bounds, level = 0) {
    this.bounds = bounds;
    this.level = level;
  }

  private split(): void {
    const midX = (this.bounds.minX + this.bounds.maxX) / 2;
    const midY = (this.bounds.minY + this.bounds.maxY) / 2;

    this.children = [
      // NW
      new Quadtree<T>(
        { minX: this.bounds.minX, minY: this.bounds.minY, maxX: midX, maxY: midY },
        this.level + 1
      ),
      // NE
      new Quadtree<T>(
        { minX: midX, minY: this.bounds.minY, maxX: this.bounds.maxX, maxY: midY },
        this.level + 1
      ),
      // SW
      new Quadtree<T>(
        { minX: this.bounds.minX, minY: midY, maxX: midX, maxY: this.bounds.maxY },
        this.level + 1
      ),
      // SE
      new Quadtree<T>(
        { minX: midX, minY: midY, maxX: this.bounds.maxX, maxY: this.bounds.maxY },
        this.level + 1
      ),
    ];
  }

  private getQuadrants(obj: QuadtreeNode): number[] {
    const midX = (this.bounds.minX + this.bounds.maxX) / 2;
    const midY = (this.bounds.minY + this.bounds.maxY) / 2;
    const quadrants: number[] = [];

    const objRight = obj.x + obj.width;
    const objBottom = obj.y + obj.height;

    const inTop = obj.y < midY;
    const inBottom = objBottom > midY;
    const inLeft = obj.x < midX;
    const inRight = objRight > midX;

    if (inTop && inLeft) quadrants.push(0); // NW
    if (inTop && inRight) quadrants.push(1); // NE
    if (inBottom && inLeft) quadrants.push(2); // SW
    if (inBottom && inRight) quadrants.push(3); // SE

    return quadrants;
  }

  insert(obj: T): void {
    if (this.children) {
      const quadrants = this.getQuadrants(obj);
      for (const q of quadrants) {
        this.children[q].insert(obj);
      }
      return;
    }

    this.objects.push(obj);

    if (this.objects.length > MAX_OBJECTS && this.level < MAX_LEVELS) {
      this.split();
      for (const existingObj of this.objects) {
        const quadrants = this.getQuadrants(existingObj);
        for (const q of quadrants) {
          this.children![q].insert(existingObj);
        }
      }
      this.objects = [];
    }
  }

  queryBounds(bounds: Bounds, seen?: Set<string>): T[] {
    const results: T[] = [];
    const seenSet = seen || new Set<string>();

    // Check if this quadrant intersects the query bounds
    if (
      this.bounds.maxX < bounds.minX ||
      this.bounds.minX > bounds.maxX ||
      this.bounds.maxY < bounds.minY ||
      this.bounds.minY > bounds.maxY
    ) {
      return results;
    }

    for (const obj of this.objects) {
      if (!seenSet.has(obj.id)) {
        const objRight = obj.x + obj.width;
        const objBottom = obj.y + obj.height;

        if (
          objRight >= bounds.minX &&
          obj.x <= bounds.maxX &&
          objBottom >= bounds.minY &&
          obj.y <= bounds.maxY
        ) {
          seenSet.add(obj.id);
          results.push(obj);
        }
      }
    }

    if (this.children) {
      for (const child of this.children) {
        results.push(...child.queryBounds(bounds, seenSet));
      }
    }

    return results;
  }

  findAtPoint(x: number, y: number): T | null {
    // Check if point is in bounds
    if (
      x < this.bounds.minX ||
      x > this.bounds.maxX ||
      y < this.bounds.minY ||
      y > this.bounds.maxY
    ) {
      return null;
    }

    // Check own objects (return last = topmost)
    let found: T | null = null;
    for (const obj of this.objects) {
      if (
        x >= obj.x &&
        x <= obj.x + obj.width &&
        y >= obj.y &&
        y <= obj.y + obj.height
      ) {
        found = obj;
      }
    }

    if (this.children) {
      for (const child of this.children) {
        const childResult = child.findAtPoint(x, y);
        if (childResult) found = childResult;
      }
    }

    return found;
  }

  static fromNodes<T extends QuadtreeNode>(nodes: T[]): Quadtree<T> {
    if (nodes.length === 0) {
      return new Quadtree<T>({ minX: 0, minY: 0, maxX: 1, maxY: 1 });
    }

    let minX = Infinity,
      minY = Infinity,
      maxX = -Infinity,
      maxY = -Infinity;

    for (const node of nodes) {
      minX = Math.min(minX, node.x);
      minY = Math.min(minY, node.y);
      maxX = Math.max(maxX, node.x + node.width);
      maxY = Math.max(maxY, node.y + node.height);
    }

    const padding = 1000;
    const tree = new Quadtree<T>({
      minX: minX - padding,
      minY: minY - padding,
      maxX: maxX + padding,
      maxY: maxY + padding,
    });

    for (const node of nodes) {
      tree.insert(node);
    }

    return tree;
  }
}
