import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { visit } from 'unist-util-visit';

const docsRoot = fileURLToPath(new URL('../../../docs/', import.meta.url));

function splitSuffix(url) {
	const match = url.match(/^([^?#]*)(.*)$/);
	return { pathname: match?.[1] ?? url, suffix: match?.[2] ?? '' };
}

export function canonicalDocUrl(sourcePath, targetUrl, base = '/lampyr/') {
	if (
		!targetUrl ||
		targetUrl.startsWith('#') ||
		targetUrl.startsWith('/') ||
		/^[a-z][a-z\d+.-]*:/i.test(targetUrl)
	) {
		return targetUrl;
	}

	const { pathname, suffix } = splitSuffix(targetUrl);
	const extension = path.extname(pathname).toLowerCase();

	if (extension !== '.md' && extension !== '.mdx') {
		return targetUrl;
	}

	const targetPath = path.resolve(path.dirname(sourcePath), pathname);
	const relativePath = path.relative(docsRoot, targetPath).replaceAll(path.sep, '/');
	if (relativePath.startsWith('../')) return targetUrl;

	let route = relativePath.replace(/\.(md|mdx)$/i, '');
	route = route.replace(/(^|\/)(README|index)$/i, '$1').replace(/\/$/, '');
	if (!route) route = 'docs';

	return `${base}${route}/${suffix}`;
}

export default function remarkCanonicalDocLinks({ base = '/lampyr/' } = {}) {
	return (tree, file) => {
		if (!file.path) return;
		const sourcePath = path.resolve(file.path);
		if (!sourcePath.startsWith(docsRoot)) return;

		visit(tree, 'link', (node) => {
			node.url = canonicalDocUrl(sourcePath, node.url, base);
		});
	};
}
