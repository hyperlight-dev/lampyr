// @ts-check
import { defineConfig } from 'astro/config';
import { unified } from '@astrojs/markdown-remark';
import starlight from '@astrojs/starlight';
import mermaid from 'astro-mermaid';
import starlightLinksValidator from 'starlight-links-validator';
import remarkCanonicalDocLinks from './src/lib/canonical-doc-links.mjs';
import { base, site } from './site-url.mjs';

const socialImage = `${site}${base}social-card.svg`;

export default defineConfig({
	site,
	base,
	trailingSlash: 'always',
	markdown: {
		processor: unified({
			remarkPlugins: [[remarkCanonicalDocLinks, { base }]],
		}),
	},
	integrations: [
		mermaid({
			autoTheme: true,
			enableLog: false,
			mermaidConfig: {
				securityLevel: 'strict',
				flowchart: { curve: 'basis', htmlLabels: false },
			},
		}),
		starlight({
			title: 'Lampyr',
			description:
				'Field guide for hardware-isolated Lampyr Services with a serverless-like Kubernetes workflow.',
			logo: {
				src: './src/assets/lampyr-mark.svg',
				alt: 'Lampyr firefly mark',
			},
			favicon: '/favicon.svg',
			social: [
				{ icon: 'github', label: 'GitHub', href: 'https://github.com/hyperlight-dev/lampyr' },
			],
			editLink: {
				baseUrl: 'https://github.com/hyperlight-dev/lampyr/edit/main/site/',
			},
			lastUpdated: true,
			pagination: true,
			credits: false,
			disable404Route: true,
			customCss: ['@fontsource-variable/albert-sans', './src/styles/docs.css'],
			plugins: [
				starlightLinksValidator({
					errorOnRelativeLinks: true,
					errorOnInvalidHashes: true,
					exclude: ({ link }) => link.startsWith(base),
				}),
			],
			head: [
				{ tag: 'meta', attrs: { property: 'og:site_name', content: 'Lampyr' } },
				{ tag: 'meta', attrs: { property: 'og:image', content: socialImage } },
				{ tag: 'meta', attrs: { property: 'og:image:alt', content: 'Lampyr: hardware-isolated services' } },
				{ tag: 'meta', attrs: { name: 'twitter:card', content: 'summary_large_image' } },
				{ tag: 'meta', attrs: { name: 'twitter:image', content: socialImage } },
			],
			sidebar: [
				{
					label: 'Install Lampyr',
					items: [
						{ label: 'Field guide', slug: 'docs' },
						{ label: 'Local requirements', slug: 'requirements' },
						{ label: 'Build from source', slug: 'contributing' },
					],
				},
				{
					label: 'Create a cluster',
					items: [
						{ label: 'Local Kind cluster', slug: 'cluster-kind' },
						{ label: 'Local k3d cluster', slug: 'cluster-k3d' },
						{ label: 'Azure AKS cluster', slug: 'cluster-aks' },
					],
				},
				{
					label: 'Build your first service',
					items: [
						{ label: 'End-to-end walkthrough', slug: 'walkthrough' },
						{ label: 'Scaffold a workspace or service', slug: 'cli/scaffold' },
						{ label: 'Run locally', slug: 'cli/dev' },
						{ label: 'Build an artifact', slug: 'cli/build' },
						{ label: 'Deploy a service', slug: 'cli/deploy' },
					],
				},
				{
					label: 'Operate your service',
					items: [
						{ label: 'Status', slug: 'cli/status' },
						{ label: 'Logs', slug: 'cli/logs' },
						{ label: 'Audit', slug: 'cli/audit' },
						{ label: 'Scale', slug: 'cli/scale' },
						{ label: 'Metrics', slug: 'observability' },
						{ label: 'Delete', slug: 'cli/delete' },
					],
				},
				{
					label: 'Advanced integrations',
					items: [
						{ label: 'Create a provider', slug: 'provider-authoring' },
						{ label: 'Azure Event Grid', slug: 'azure-event-grid-source' },
						{ label: 'Events and sources', slug: 'cli/event' },
					],
				},
				{
					label: 'Reference',
					items: [
						{ label: 'CLI commands', slug: 'cli' },
						{ label: 'Lampyr Service model', slug: 'lampyr-service' },
						{ label: 'Runtime examples', slug: 'examples' },
						{ label: 'Runtime compatibility', slug: 'runtime-compatibility' },
						{ label: 'Cluster command', slug: 'cli/cluster' },
						{ label: 'Push an image separately', slug: 'cli/push' },
						{ label: 'Verify artifacts', slug: 'cli/verify' },
						{ label: 'Platform setup', slug: 'cli/setup' },
						{ label: 'Host check', slug: 'cli/doctor' },
						{ label: 'CLI version', slug: 'cli/version' },
					],
				},
				{
					label: 'Designs and proposals',
					collapsed: true,
					items: [
						{ label: 'Confirmed product design', slug: 'designs/product-design' },
						{ label: 'Exploratory project plan', slug: 'designs/project-plan' },
					],
				},
			],
		}),
	],
});
