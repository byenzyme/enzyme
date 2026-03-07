// Layout Web Worker: off-thread position computation for explore canvas

import type { LayoutWorkerInput, LayoutWorkerOutput } from "../components/explore/types";
import { computeLayout } from "../components/explore/layout";

self.onmessage = (e: MessageEvent<LayoutWorkerInput>) => {
  if (e.data.type !== "compute_layout") return;

  const positions = computeLayout(e.data.docs);

  const output: LayoutWorkerOutput = {
    type: "layout_complete",
    positions: positions.map((p) => ({
      id: p.id,
      filePath: p.filePath,
      x: p.x,
      y: p.y,
      width: p.width,
      height: p.height,
      docId: p.docId,
      chunkIndex: p.chunkIndex,
    })),
  };

  // Attach timelineProgress (used by renderer for entrance stagger)
  for (let i = 0; i < output.positions.length; i++) {
    (output.positions[i] as any).timelineProgress = positions[i].timelineProgress;
  }

  self.postMessage(output);
};
