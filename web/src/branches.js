const DEFAULT_COLUMN_GAP = 22;
const DEFAULT_ROW_GAP = 34;
const DEFAULT_PADDING = 18;

export function layoutBranchGraph(
  nodes,
  {
    columnGap = DEFAULT_COLUMN_GAP,
    rowGap = DEFAULT_ROW_GAP,
    padding = DEFAULT_PADDING
  } = {}
) {
  const source = Array.isArray(nodes) ? nodes : [];
  const ids = new Set(source.map((node) => node.id));
  const children = new Map();
  for (const node of source) {
    const parentId = ids.has(node.parent_id) ? node.parent_id : null;
    const siblings = children.get(parentId) ?? [];
    siblings.push(node.id);
    children.set(parentId, siblings);
  }

  const rows = [];
  const visited = new Set();
  function visit(id, depth) {
    if (visited.has(id)) return;
    visited.add(id);
    rows.push({
      id,
      parentId: source.find((node) => node.id === id)?.parent_id ?? null,
      depth,
      row: rows.length
    });
    for (const child of children.get(id) ?? []) visit(child, depth + 1);
  }
  for (const root of children.get(null) ?? []) visit(root, 0);
  for (const node of source) visit(node.id, 0);

  const positioned = rows.map((node) => ({
    ...node,
    x: padding + node.depth * columnGap,
    y: padding + node.row * rowGap
  }));
  const byId = new Map(positioned.map((node) => [node.id, node]));
  const edges = positioned.flatMap((node) => {
    const parent = byId.get(node.parentId);
    return parent ? [{ parent, child: node }] : [];
  });
  const maxDepth = positioned.reduce(
    (maximum, node) => Math.max(maximum, node.depth),
    0
  );
  return {
    nodes: positioned,
    edges,
    width: padding * 2 + maxDepth * columnGap + 12,
    height: Math.max(48, padding * 2 + Math.max(0, rows.length - 1) * rowGap)
  };
}

export function branchEdgePath(edge) {
  const middle = (edge.parent.x + edge.child.x) / 2;
  return `M ${edge.parent.x} ${edge.parent.y} C ${middle} ${edge.parent.y}, ${middle} ${edge.child.y}, ${edge.child.x} ${edge.child.y}`;
}

export function routeContainsEdge(route, edge) {
  return route.has(edge.parent.id) && route.has(edge.child.id);
}

export function latestActiveBranchNodeId(nodes, activeNodeIds) {
  const active = new Set(activeNodeIds ?? []);
  return (
    [...(nodes ?? [])].reverse().find((node) => active.has(node.id))?.id ??
    null
  );
}

export function projectEditedSession(session, nodeId, content, createdAt) {
  const messageIndex = session?.active_message_ids?.indexOf(nodeId) ?? -1;
  if (!session || messageIndex < 0) return null;
  return {
    ...session,
    title: messageIndex === 0 ? content.slice(0, 72) : session.title,
    messages: [
      ...(session.messages ?? []).slice(0, messageIndex),
      {
        role: "user",
        content,
        created_at: createdAt
      }
    ],
    active_message_ids: [
      ...(session.active_message_ids ?? []).slice(0, messageIndex),
      `editing-${nodeId}`
    ],
    activities: (session.activities ?? []).filter(
      (activity) => activity.turn_message_index < messageIndex
    ),
    turns: (session.turns ?? []).filter(
      (turn) => turn.message_index < messageIndex
    )
  };
}
