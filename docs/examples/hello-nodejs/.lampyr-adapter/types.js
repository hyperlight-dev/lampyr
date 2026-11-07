class HandlerError extends Error {
  constructor(status, message) {
    super(message);
    if (!Number.isInteger(status) || status < 400 || status > 599) {
      throw new TypeError('HandlerError status must be between 400 and 599');
    }
    this.status = status;
  }
}

class Headers {
  constructor(entries = []) {
    this.entries = Array.from(entries, ([name, value]) => [name.toLowerCase(), value]);
  }
  get(name) { return this.getAll(name)[0]; }
  getAll(name) { return this.entries.filter(([key]) => key === name.toLowerCase()).map(([, value]) => value); }
  append(name, value) { this.entries.push([name.toLowerCase(), value]); }
  set(name, value) {
    this.entries = this.entries.filter(([key]) => key !== name.toLowerCase());
    this.append(name, value);
  }
  [Symbol.iterator]() { return this.entries[Symbol.iterator](); }
}

class Body {
  constructor(source = [], limit = Infinity) {
    this.source = source;
    this.limit = limit;
    this.used = false;
    this.closed = false;
    this.iterator = null;
  }
  async *[Symbol.asyncIterator]() {
    if (this.used) throw new TypeError('body has already been consumed');
    this.used = true;
    const iterator = this.source[Symbol.asyncIterator]?.() ?? this.source[Symbol.iterator]();
    this.iterator = iterator;
    let size = 0;
    try {
      while (true) {
        const {value, done} = await iterator.next();
        if (done) return;
        if (!(value instanceof Uint8Array)) throw new TypeError('body chunks must be bytes');
        size += value.byteLength;
        if (size > this.limit) throw new HandlerError(413, 'request body too large');
        yield value;
      }
    } finally {
      await this.close();
    }
  }
  async close() {
    if (this.closed) return;
    this.closed = true;
    const iterator = this.iterator ?? this.source;
    this.iterator = null;
    this.used = true;
    await iterator?.return?.();
  }
  async bytes(limit = 1024 * 1024) {
    const chunks = [];
    let size = 0;
    for await (const chunk of this) {
      size += chunk.byteLength;
      if (size > limit) throw new HandlerError(413, 'body read limit exceeded');
      chunks.push(chunk);
    }
    return Buffer.concat(chunks, size);
  }
  async text(limit) {
    const bytes = await this.bytes(limit);
    try { return new TextDecoder('utf-8', {fatal: true}).decode(bytes); }
    catch { throw new HandlerError(400, 'invalid UTF-8 body'); }
  }
  async json(limit) {
    const text = await this.text(limit);
    try { return JSON.parse(text); }
    catch { throw new HandlerError(400, 'invalid JSON body'); }
  }
}

class Response {
  constructor(body = new Body(), {status = 200, headers = new Headers()} = {}) {
    this.body = body;
    this.status = status;
    this.headers = headers;
  }
  static bytes(value, options) { return new Response(new Body([value]), options); }
  static text(value, options = {}) {
    const headers = new Headers(options.headers);
    if (!headers.get('content-type')) headers.set('content-type', 'text/plain; charset=utf-8');
    return Response.bytes(Buffer.from(value), {...options, headers});
  }
  static json(value, options = {}) {
    const headers = new Headers(options.headers);
    if (!headers.get('content-type')) headers.set('content-type', 'application/json');
    return Response.bytes(Buffer.from(JSON.stringify(value)), {...options, headers});
  }
}
module.exports = {Body, Headers, Response, HandlerError};
