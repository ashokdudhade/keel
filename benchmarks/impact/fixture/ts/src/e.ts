export function Logged(target: any) {
  return target;
}

@Logged
export class Service {
  run() {}
}
