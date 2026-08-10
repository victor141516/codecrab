export function toggleProjectExpansion(expanded, root) {
  const next = new Set(expanded);
  if (!next.delete(root)) next.add(root);
  return next;
}

export function expandKnownProject(expanded, projects, root) {
  const next = new Set(
    [...expanded].filter((candidate) =>
      projects.some((project) => project.root === candidate)
    )
  );
  if (projects.some((project) => project.root === root)) next.add(root);
  return next;
}

export function newSessionPayload(projectRoot) {
  return projectRoot == null ? { no_project: true } : { project: projectRoot };
}

export function toggleSessionExpansion(collapsed, sessionId) {
  const next = new Set(collapsed);
  if (!next.delete(sessionId)) next.add(sessionId);
  return next;
}

export function visibleSessionRows(sessions, collapsed) {
  const visible = [];
  let hiddenBelowDepth = null;
  for (const session of sessions) {
    if (hiddenBelowDepth !== null) {
      if (session.depth > hiddenBelowDepth) continue;
      hiddenBelowDepth = null;
    }
    visible.push(session);
    if (session.descendant_count > 0 && collapsed.has(session.id)) {
      hiddenBelowDepth = session.depth;
    }
  }
  return visible;
}
