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
  return { project: projectRoot };
}
