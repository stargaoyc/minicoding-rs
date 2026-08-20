/** MSW server 实例（handlers 见 `handlers.ts`）。 */
import { setupServer } from "msw/node";
import { handlers } from "./handlers";

export const server = setupServer(...handlers);
