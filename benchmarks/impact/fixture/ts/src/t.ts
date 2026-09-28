export async function boot() {
  const m = await import("./s");
  return m.lazy();
}
