import { render } from "solid-js/web";
import "leaflet/dist/leaflet.css";
import "./styles.css";
import { Leaderboard } from "./Leaderboard";

render(() => <Leaderboard />, document.getElementById("root")!);
