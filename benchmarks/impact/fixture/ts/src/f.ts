export interface Shape {
  area(): number;
}

export class Circle implements Shape {
  area() {
    return 1;
  }
}
