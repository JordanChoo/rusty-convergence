import RustWorker from "./build/index.js";

export * from "./build/index.js";

export default class extends RustWorker {
  fetch(request) {
    return Promise.resolve(super.fetch(request));
  }
}
