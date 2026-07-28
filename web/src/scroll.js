export function isScrolledToBottom(element, threshold = 4) {
  return (
    element.scrollHeight - element.clientHeight - element.scrollTop <= threshold
  );
}
