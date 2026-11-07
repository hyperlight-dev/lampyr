import assert from 'node:assert/strict';
import test from 'node:test';
import { fileURLToPath } from 'node:url';
import { canonicalDocUrl } from './canonical-doc-links.mjs';

const docs = fileURLToPath(new URL('../../../docs/', import.meta.url));

test('rewrites sibling Markdown links to base-aware routes', () => {
	assert.equal(
		canonicalDocUrl(`${docs}walkthrough.md`, 'runtime-compatibility.md'),
		'/lampyr/runtime-compatibility/',
	);
});

test('maps README documents to their directory route', () => {
	assert.equal(canonicalDocUrl(`${docs}index.md`, 'cli/README.md'), '/lampyr/cli/');
});

test('rewrites links to and from nested design documents', () => {
	assert.equal(
		canonicalDocUrl(`${docs}index.md`, 'designs/product-design.md'),
		'/lampyr/designs/product-design/',
	);
	assert.equal(
		canonicalDocUrl(`${docs}designs/project-plan.md`, '../walkthrough.md'),
		'/lampyr/walkthrough/',
	);
	assert.equal(
		canonicalDocUrl(`${docs}cli/setup.md`, '../contributing.md#build-from-source'),
		'/lampyr/contributing/#build-from-source',
	);
});

test('leaves external and fragment links unchanged', () => {
	assert.equal(canonicalDocUrl(`${docs}index.md`, 'https://example.com'), 'https://example.com');
	assert.equal(canonicalDocUrl(`${docs}index.md`, '#preview'), '#preview');
});
