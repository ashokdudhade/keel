import { Entity } from "./l";

export type MaybeEntity<T> = T extends Entity ? T : never;
