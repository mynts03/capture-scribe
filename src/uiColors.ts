import tokens from "./ui-colors.json" with { type: "json" };

// Static imports are bundled for both main and history windows; no runtime IO.
export const uiColors = tokens;
export function colorVariables(): Record<string, string> {
  return Object.fromEntries(Object.entries(tokens).map(([role, value]) =>
    ["--" + role.replace(/[A-Z]/g, letter => "-" + letter.toLowerCase()), value]));
}
export function applyUIColors(element: Pick<HTMLElement, "style">): void {
  for (const [role, value] of Object.entries(colorVariables())) element.style.setProperty(role, value);
}
