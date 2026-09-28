import { Config } from "./j.js";

export function getPort(c) {
  return c.port;
}

export function hasPort(c) {
  return c.port > 0;
}
