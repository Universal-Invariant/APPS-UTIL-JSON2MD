// {{upper text}} → "HELLO WORLD"
function upper(text) {
    console.log("upper called with:", text);  // Debug log
    return String(text || '').toUpperCase();
}

// {{repeat text times}} → "abcabcabc"
function repeat(text, times) {
    console.log("repeat called with:", text, times);  // Debug log
    const t = String(text || '');
    const n = parseInt(times) || 1;
    return t.repeat(Math.max(0, n));
}

// {{wrap text}} → "[hello world]"
function wrap(text) {
    return "[" + String(text || '').trim() + "]";
}