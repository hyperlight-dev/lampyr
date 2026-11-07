import { access, readFile, readdir, stat } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import { base } from '../site-url.mjs';

const dist = fileURLToPath(new URL('../dist/', import.meta.url));
const expectedFiles = [
	'index.html',
	'404.html',
	'docs/index.html',
	'walkthrough/index.html',
	'contributing/index.html',
	'cluster-kind/index.html',
	'cluster-k3d/index.html',
	'cluster-aks/index.html',
	'designs/product-design/index.html',
	'designs/project-plan/index.html',
	'lampyr-service/index.html',
	'cli/index.html',
	'cli/deploy/index.html',
	'runtime-compatibility/index.html',
	'sitemap-index.xml',
];

for (const relativePath of expectedFiles) {
	await access(`${dist}${relativePath}`);
}

async function* htmlFiles(directory) {
	for (const entry of await readdir(directory, { withFileTypes: true })) {
		const entryPath = `${directory}/${entry.name}`;
		if (entry.isDirectory()) yield* htmlFiles(entryPath);
		else if (entry.name.endsWith('.html')) yield entryPath;
	}
}

const invalidBaseReferences = [];
const invalidLinks = [];
const htmlCache = new Map();

async function getHtml(filePath) {
	if (!htmlCache.has(filePath)) htmlCache.set(filePath, await readFile(filePath, 'utf8'));
	return htmlCache.get(filePath);
}

async function pathExists(filePath) {
	try {
		await stat(filePath);
		return true;
	} catch {
		return false;
	}
}

function targetFile(urlPath) {
	const relativePath = decodeURIComponent(urlPath.slice(base.length)).replace(/^\/+/, '');
	if (!relativePath) return `${dist}index.html`;
	if (relativePath.endsWith('/')) return `${dist}${relativePath}index.html`;
	return `${dist}${relativePath}`;
}

for await (const htmlFile of htmlFiles(dist.slice(0, -1))) {
	const html = await getHtml(htmlFile);
	for (const match of html.matchAll(/\b(?:href|src)="(\/[^"]*)"/g)) {
		if (!match[1].startsWith(base)) {
			invalidBaseReferences.push(`${htmlFile.slice(dist.length)}: ${match[1]}`);
		}
	}

	for (const match of html.matchAll(/\bhref="([^"]+)"/g)) {
		const href = match[1];
		if (href.startsWith('#')) {
			const id = decodeURIComponent(href.slice(1));
			if (id && !html.includes(`id="${id}"`)) {
				invalidLinks.push(`${htmlFile.slice(dist.length)}: missing #${id}`);
			}
			continue;
		}
		if (!href.startsWith(base)) continue;

		const url = new URL(href, 'https://example.invalid');
		const filePath = targetFile(url.pathname);
		if (!(await pathExists(filePath))) {
			invalidLinks.push(`${htmlFile.slice(dist.length)}: missing ${href}`);
			continue;
		}
		if (url.hash && filePath.endsWith('.html')) {
			const targetHtml = await getHtml(filePath);
			const id = decodeURIComponent(url.hash.slice(1));
			if (id && !targetHtml.includes(`id="${id}"`)) {
				invalidLinks.push(`${htmlFile.slice(dist.length)}: missing ${href}`);
			}
		}
	}
}

if (invalidBaseReferences.length > 0) {
	throw new Error(`Found project-root URLs without the ${base} base:\n${invalidBaseReferences.join('\n')}`);
}

if (invalidLinks.length > 0) {
	throw new Error(`Found invalid generated website links:\n${invalidLinks.join('\n')}`);
}

console.log(`Validated ${expectedFiles.length} required artifacts, base paths, routes, and anchors.`);
