import { LIMIT } from "./h";

export function check(n: number): boolean {
  return n < LIMIT;
}

export function doubled(): number {
  return LIMIT * 2;
}
