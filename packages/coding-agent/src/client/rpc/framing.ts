import { StringDecoder } from "node:string_decoder";

/**
 * Attach a strict LF-only newline-delimited line reader to a byte stream.
 *
 * Intentionally avoids Node's readline, which splits on U+2028/U+2029 too; JSON
 * strings may contain those, so splitting on them corrupts our frames.
 */
export function readLines(stream: NodeJS.ReadableStream, onLine: (line: string) => void): () => void {
	const decoder = new StringDecoder("utf8");
	let buffer = "";

	const emit = (line: string) => {
		onLine(line.endsWith("\r") ? line.slice(0, -1) : line);
	};

	const onData = (chunk: string | Buffer) => {
		buffer += typeof chunk === "string" ? chunk : decoder.write(chunk);
		while (true) {
			const newline = buffer.indexOf("\n");
			if (newline === -1) return;
			emit(buffer.slice(0, newline));
			buffer = buffer.slice(newline + 1);
		}
	};

	const onEnd = () => {
		buffer += decoder.end();
		if (buffer.length > 0) {
			emit(buffer);
			buffer = "";
		}
	};

	stream.on("data", onData);
	stream.on("end", onEnd);

	return () => {
		stream.off("data", onData);
		stream.off("end", onEnd);
	};
}
