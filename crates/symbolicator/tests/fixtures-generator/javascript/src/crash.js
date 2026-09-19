export function fail(message) {
  throw new Error(message);
}

export function run() {
  fail('fixture crash');
}
