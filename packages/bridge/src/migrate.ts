// Standalone migration runner: node src/migrate.ts
import { migrate, closeDb } from "./db.ts";

const applied = await migrate();
console.log(applied.length ? `applied: ${applied.join(", ")}` : "schema up to date");
await closeDb();
