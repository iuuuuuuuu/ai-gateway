/** 事件目标是否是可编辑文本控件（输入框 / 文本域 / 下拉框 / contenteditable）。 */
export function isTextEditableTarget(target: EventTarget | null): boolean {
  if (!(target instanceof HTMLElement)) return false;
  const tag = target.tagName.toLowerCase();
  if (tag === "input" || tag === "textarea" || tag === "select") return true;
  return target.isContentEditable;
}