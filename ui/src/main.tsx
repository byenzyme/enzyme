import "@fontsource/geist-sans/400.css";
import "@fontsource/geist-sans/500.css";
import "./index.css";

import { createRoot } from "react-dom/client";
import { ExplorePage } from "./ExplorePage";

createRoot(document.getElementById("root")!).render(<ExplorePage />);
