import { Config } from "./j";

export function getPort(c: Config): number {
  return c.port;
}

export function hasPort(c: Config): boolean {
  return c.port > 0;
}
