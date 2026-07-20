export class TsService {
  run(): void {
    helper();
  }
}

export function helper(): void {}

// False friend for naive patterns:
const note = "export function helper(): void {}";
