export const VIRTUAL_TIMELINE_OVERSCAN = 640;
export const VIRTUAL_TIMELINE_DEFAULT_HEIGHT = 104;

export class VirtualTimelineModel {
  constructor() {
    this.keys = [];
    this.descriptors = new Map();
    this.cache = new Map();
    this.indexByKey = new Map();
    this.heights = [];
    this.tree = new FenwickTree([]);
  }

  setItems(descriptors) {
    const keys = descriptors.map((descriptor) => descriptor.key);
    const liveKeys = new Set(keys);
    for (const key of this.cache.keys()) {
      if (!liveKeys.has(key)) this.cache.delete(key);
    }

    const sameKeys =
      keys.length === this.keys.length &&
      keys.every((key, index) => key === this.keys[index]);
    for (const descriptor of descriptors) {
      const estimate = normalizedHeight(descriptor.estimate);
      const cached = this.cache.get(descriptor.key);
      if (!cached) {
        this.cache.set(descriptor.key, {
          height: estimate,
          estimate,
          measured: false,
          signature: descriptor.signature
        });
      } else {
        cached.estimate = estimate;
        if (cached.signature !== descriptor.signature) {
          cached.signature = descriptor.signature;
          if (!descriptor.mounted) {
            cached.height = estimate;
            cached.measured = false;
          }
        }
      }
    }
    this.descriptors = new Map(
      descriptors.map((descriptor) => [descriptor.key, descriptor])
    );

    if (!sameKeys) {
      this.keys = keys;
      this.indexByKey = new Map(keys.map((key, index) => [key, index]));
      this.heights = keys.map(
        (key) =>
          this.cache.get(key)?.height ?? VIRTUAL_TIMELINE_DEFAULT_HEIGHT
      );
      this.tree = new FenwickTree(this.heights);
      return true;
    }

    let changed = false;
    for (const [index, key] of this.keys.entries()) {
      const height =
        this.cache.get(key)?.height ?? VIRTUAL_TIMELINE_DEFAULT_HEIGHT;
      if (height !== this.heights[index]) {
        this.tree.add(index, height - this.heights[index]);
        this.heights[index] = height;
        changed = true;
      }
    }
    return changed;
  }

  measure(key, height) {
    const index = this.indexByKey.get(key);
    if (index == null) return { changed: false, delta: 0, index: -1 };
    const nextHeight = normalizedHeight(height);
    const previousHeight = this.heights[index];
    const delta = nextHeight - previousHeight;
    const cached = this.cache.get(key);
    if (cached) {
      cached.height = nextHeight;
      cached.measured = true;
    }
    if (Math.abs(delta) < 0.5) {
      return { changed: false, delta: 0, index };
    }
    this.heights[index] = nextHeight;
    this.tree.add(index, delta);
    return { changed: true, delta, index };
  }

  updateItem(descriptor) {
    const index = this.indexByKey.get(descriptor.key);
    if (index == null) return false;
    const cached = this.cache.get(descriptor.key);
    if (!cached) return false;
    const estimate = normalizedHeight(descriptor.estimate);
    cached.estimate = estimate;
    if (cached.signature === descriptor.signature) return false;
    cached.signature = descriptor.signature;
    if (descriptor.mounted) return false;
    cached.measured = false;
    cached.height = estimate;
    const delta = estimate - this.heights[index];
    if (Math.abs(delta) < 0.5) return false;
    this.heights[index] = estimate;
    this.tree.add(index, delta);
    return true;
  }

  invalidateMeasurements() {
    let changed = false;
    for (const [index, key] of this.keys.entries()) {
      const cached = this.cache.get(key);
      if (!cached) continue;
      cached.measured = false;
      cached.height = cached.estimate;
      const delta = cached.height - this.heights[index];
      if (Math.abs(delta) < 0.5) continue;
      this.heights[index] = cached.height;
      this.tree.add(index, delta);
      changed = true;
    }
    return changed;
  }

  range(scrollTop, viewportHeight, overscan = VIRTUAL_TIMELINE_OVERSCAN) {
    if (!this.keys.length) {
      return {
        start: 0,
        end: 0,
        top: 0,
        bottom: 0,
        total: 0
      };
    }
    const total = this.totalHeight();
    const viewportStart = clamp(scrollTop, 0, total);
    const viewportEnd = clamp(
      viewportStart + Math.max(0, viewportHeight),
      0,
      total
    );
    const startOffset = Math.max(0, viewportStart - overscan);
    const endOffset = Math.min(total, viewportEnd + overscan);
    const start = this.indexAtOffset(startOffset);
    const end =
      endOffset >= total
        ? this.keys.length
        : Math.min(this.keys.length, this.indexAtOffset(endOffset) + 1);
    const top = this.offsetForIndex(start);
    const renderedEnd = this.offsetForIndex(end);
    return {
      start,
      end,
      top,
      bottom: Math.max(0, total - renderedEnd),
      total
    };
  }

  anchorAt(scrollTop) {
    if (!this.keys.length) return null;
    const index = this.indexAtOffset(
      clamp(scrollTop, 0, Math.max(0, this.totalHeight() - 1))
    );
    return {
      key: this.keys[index],
      offset: scrollTop - this.offsetForIndex(index)
    };
  }

  scrollTopForAnchor(anchor) {
    if (!anchor) return 0;
    const index = this.indexByKey.get(anchor.key);
    if (index == null) return 0;
    return clamp(
      this.offsetForIndex(index) + anchor.offset,
      0,
      Math.max(0, this.totalHeight())
    );
  }

  scrollTopToRevealIndex(
    index,
    scrollTop,
    viewportHeight,
    margin = 0
  ) {
    if (!this.keys.length) return 0;
    const safeIndex = clamp(index, 0, this.keys.length - 1);
    const height = Math.max(1, viewportHeight);
    const inset = clamp(margin, 0, height / 2);
    const maximum = Math.max(0, this.totalHeight() - height);
    const current = clamp(scrollTop, 0, maximum);
    const itemStart = this.offsetForIndex(safeIndex);
    const itemEnd = this.offsetForIndex(safeIndex + 1);
    const available = Math.max(1, height - inset * 2);

    if (itemEnd - itemStart > available || itemStart < current + inset) {
      return clamp(itemStart - inset, 0, maximum);
    }
    if (itemEnd > current + height - inset) {
      return clamp(itemEnd - height + inset, 0, maximum);
    }
    return current;
  }

  offsetForIndex(index) {
    return this.tree.sum(clamp(index, 0, this.keys.length));
  }

  indexAtOffset(offset) {
    return this.tree.indexAtOffset(offset);
  }

  totalHeight() {
    return this.tree.sum(this.keys.length);
  }
}

export class VirtualTimelineStore {
  constructor() {
    this.models = new Map();
    this.views = new Map();
    this.activeSessionId = null;
  }

  activate(sessionId, descriptors) {
    this.activeSessionId = sessionId ?? null;
    if (!sessionId) return null;
    if (!this.models.has(sessionId)) {
      this.models.set(sessionId, new VirtualTimelineModel());
    }
    const model = this.models.get(sessionId);
    model.setItems(descriptors);
    return model;
  }

  active() {
    return this.activeSessionId
      ? this.models.get(this.activeSessionId) ?? null
      : null;
  }

  saveView(sessionId, view) {
    if (sessionId) this.views.set(sessionId, view);
  }

  view(sessionId) {
    return sessionId ? this.views.get(sessionId) ?? null : null;
  }

  delete(sessionId) {
    this.models.delete(sessionId);
    this.views.delete(sessionId);
    if (this.activeSessionId === sessionId) this.activeSessionId = null;
  }
}

export function createVirtualTimelineFixture(count, height = 96) {
  return Array.from({ length: count }, (_, index) => ({
    key: `fixture-${index}`,
    estimate: height,
    signature: `fixture-${index}`,
    mounted: false
  }));
}

class FenwickTree {
  constructor(values) {
    this.length = values.length;
    this.tree = Array(this.length + 1).fill(0);
    for (const [index, value] of values.entries()) this.add(index, value);
  }

  add(index, delta) {
    for (
      let treeIndex = index + 1;
      treeIndex <= this.length;
      treeIndex += treeIndex & -treeIndex
    ) {
      this.tree[treeIndex] += delta;
    }
  }

  sum(end) {
    let total = 0;
    for (
      let treeIndex = end;
      treeIndex > 0;
      treeIndex -= treeIndex & -treeIndex
    ) {
      total += this.tree[treeIndex];
    }
    return total;
  }

  indexAtOffset(offset) {
    if (!this.length) return 0;
    const target = Math.max(0, offset);
    let index = 0;
    let prefix = 0;
    let step = 1;
    while (step * 2 <= this.length) step *= 2;
    for (; step > 0; step = Math.floor(step / 2)) {
      const next = index + step;
      if (next <= this.length && prefix + this.tree[next] <= target) {
        index = next;
        prefix += this.tree[next];
      }
    }
    return Math.min(index, this.length - 1);
  }
}

function normalizedHeight(height) {
  return Math.max(1, Number.isFinite(height) ? height : VIRTUAL_TIMELINE_DEFAULT_HEIGHT);
}

function clamp(value, minimum, maximum) {
  return Math.min(maximum, Math.max(minimum, value));
}
