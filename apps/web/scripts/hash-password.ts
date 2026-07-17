// Generate ADMIN_PASSWORD_HASH: pnpm exec tsx scripts/hash-password.ts <password>
import { randomBytes, scryptSync } from 'node:crypto';

const password = process.argv[2];
if (!password) {
	console.error('usage: tsx scripts/hash-password.ts <password>');
	process.exit(1);
}
const salt = randomBytes(16).toString('hex');
console.log(`${salt}:${scryptSync(password, salt, 32).toString('hex')}`);
