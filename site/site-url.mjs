// Where the site is served. The Pages workflow passes what actions/configure-pages
// reports, since a private Pages site is served from the root of its own host
// rather than under /lampyr/; local builds default to the public site.
const origin = process.env.SITE_ORIGIN || 'https://hyperlight-dev.github.io';
const basePath = process.env.SITE_BASE_PATH ?? '/lampyr';

export const site = origin.replace(/\/+$/, '');
export const base = `/${basePath.replace(/^\/+|\/+$/g, '')}/`.replace(/^\/\/$/, '/');
