/// <reference lib="es2022" />
/// <reference lib="dom" />
export class Headers implements Iterable<[string, string]> {
  constructor(entries?: Iterable<readonly [string, string]>);
  get(name: string): string | undefined;
  getAll(name: string): string[];
  append(name: string, value: string): void;
  set(name: string, value: string): void;
  [Symbol.iterator](): Iterator<[string, string]>;
}
export class Body implements AsyncIterable<Uint8Array> {
  constructor(source?: Iterable<Uint8Array> | AsyncIterable<Uint8Array>, limit?: number);
  bytes(limit?: number): Promise<Uint8Array>;
  text(limit?: number): Promise<string>;
  json(limit?: number): Promise<Value>;
  close(): Promise<void>;
  [Symbol.asyncIterator](): AsyncIterator<Uint8Array>;
}
export interface RequestHead {
  method: string;
  path: string;
  headers: Headers;
}
export interface Request extends RequestHead {
  body: Body;
}
export interface Context {
  signal: AbortSignal;
}
export interface ProtocolContext extends Context {
  request: RequestHead;
}
export type Chunk = string | Uint8Array;
export type Value = null | boolean | number | string | Value[] | {[key: string]: Value};
export interface ResponseOptions {
  status?: number;
  headers?: Headers;
}
export class Response {
  constructor(body?: Body, options?: ResponseOptions);
  status: number;
  headers: Headers;
  body: Body;
  static bytes(value: Uint8Array, options?: ResponseOptions): Response;
  static text(value: string, options?: ResponseOptions): Response;
  static json(value: Value, options?: ResponseOptions): Response;
}
export interface Event {
  [attribute: string]: Value;
  id: string;
  type: string;
  source: string;
  subject: string;
  time: string;
  attempt: number;
  data: Value;
}
export interface WebSocketSession extends AsyncIterable<Chunk> {
  send(data: Chunk): Promise<void>;
  close(code?: number, reason?: string): Promise<void>;
}
export class HandlerError extends Error {
  constructor(status: number, message: string);
  status: number;
}
export interface Hooks {
  handle(request: Request, context: Context): Response | Promise<Response>;
  handleEvent?(event: Event, context: ProtocolContext): Response | Promise<Response>;
  handleWebSocket?(session: WebSocketSession, context: ProtocolContext): void | Promise<void>;
}
