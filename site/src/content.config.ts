import { defineCollection } from 'astro:content';
import { glob } from 'astro/loaders';
import { docsSchema } from '@astrojs/starlight/schema';

export const collections = {
	docs: defineCollection({
		loader: glob({
			base: '../docs',
			pattern: ['**/*.{md,mdx}'],
			generateId: ({ entry }) => {
				const id = entry.replace(/\.(md|mdx)$/, '').replace(/\/README$/, '');
				return id === 'index' ? 'docs' : id;
			},
		}),
		schema: docsSchema(),
	}),
};
