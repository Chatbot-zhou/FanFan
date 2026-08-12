import type { ReactNode } from "react";

/** 从问题中提取用于高亮的关键词（去掉语气词，保留 2 字以上词，长词优先） */
export const extractQuestionTerms = (question: string): string[] => {
  const terms = (question.match(/[\p{L}\p{N}]{2,}/gu) ?? [])
    .flatMap((value) => value.split(/(?:关于|有关|哪些|什么|如何|是否|请问|请|的|了|是|在|中|和|与)+/u))
    .map((value) => value.trim())
    .filter((value) => value.length >= 2)
    .sort((left, right) => right.length - left.length);
  return [...new Set(terms)];
};

/** 纯文本关键词高亮（摘要/原文区用，不渲染 markdown，避免把原文符号当作格式） */
export const highlightPlainTerms = (text: string, question: string): ReactNode => {
  const unique = extractQuestionTerms(question);
  if (!unique.length) return text;
  const expression = new RegExp(`(${unique.map((value) => value.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")).join("|")})`, "giu");
  const normalized = new Set(unique.map((value) => value.toLocaleLowerCase("zh-CN")));
  return text.split(expression).map((part, index) => normalized.has(part.toLocaleLowerCase("zh-CN"))
    ? <strong className="answer-keyword" key={`${part}-${index}`}>{part}</strong>
    : part);
};
