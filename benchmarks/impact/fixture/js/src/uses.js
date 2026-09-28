import { LIMIT } from "./limit.js";

export function check(n) {
  return n < LIMIT;
}

export function doubled() {
  return LIMIT * 2;
}
