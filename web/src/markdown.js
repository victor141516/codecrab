import DOMPurify from "dompurify";
import hljs from "highlight.js/lib/common";
import { Marked } from "marked";
import { markedHighlight } from "marked-highlight";

const markdown = new Marked(
  markedHighlight({
    emptyLangClass: "hljs",
    langPrefix: "hljs language-",
    highlight(code, language) {
      const resolved = hljs.getLanguage(language) ? language : "plaintext";
      return hljs.highlight(code, { language: resolved }).value;
    }
  })
);

markdown.setOptions({
  breaks: true,
  gfm: true
});

export function renderMarkdown(source) {
  const html = markdown.parse(source ?? "");
  return DOMPurify.sanitize(html, {
    USE_PROFILES: { html: true }
  });
}
