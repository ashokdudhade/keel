export type Token = string;

export function parse(raw: unknown) {
  return raw as Token;
}
