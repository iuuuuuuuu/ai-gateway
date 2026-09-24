package qoder

import (
	"encoding/json"
	"io"
	"strings"
	"testing"
)

func nestedToolFrame(t *testing.T, chunk map[string]any) string {
	t.Helper()
	inner, err := json.Marshal(map[string]any{"choices": []any{chunk}})
	if err != nil {
		t.Fatal(err)
	}
	outer, err := json.Marshal(map[string]string{"body": string(inner)})
	if err != nil {
		t.Fatal(err)
	}
	return "data: " + string(outer)
}

func TestNestedToolCallsAreForwarded(t *testing.T) {
	stream := strings.Join([]string{
		nestedToolFrame(t, map[string]any{"delta": map[string]any{"tool_calls": []any{map[string]any{
			"function": map[string]any{"arguments": "", "name": "probe_echo"}, "id": "call-1", "index": 0, "type": "function",
		}}}}),
		nestedToolFrame(t, map[string]any{"delta": map[string]any{"tool_calls": []any{map[string]any{
			"function": map[string]any{"arguments": `{"value": "`}, "id": "", "index": 0, "type": "function",
		}}}}),
		nestedToolFrame(t, map[string]any{"delta": map[string]any{"tool_calls": []any{map[string]any{
			"function": map[string]any{"arguments": `hello"}`}, "id": "", "index": 0, "type": "function",
		}}}}),
		nestedToolFrame(t, map[string]any{"delta": map[string]any{}, "finish_reason": "tool_calls"}),
		"data: [DONE]",
	}, "\n")
	out := NewOpenAIStream(ioNopCloser{Reader: strings.NewReader(stream)}, "Qwen3.8-Flash")
	b, err := io.ReadAll(out)
	if err != nil {
		t.Fatal(err)
	}
	s := string(b)
	if !strings.Contains(s, `"tool_calls"`) || !strings.Contains(s, `probe_echo`) || !strings.Contains(s, `value`) {
		t.Fatalf("工具调用、工具名和 arguments 必须被透传，实际=%s", s)
	}
	if !strings.Contains(s, `"finish_reason":"tool_calls"`) {
		t.Fatalf("工具调用结束原因必须保留，实际=%s", s)
	}
}

type ioNopCloser struct{ io.Reader }

func (ioNopCloser) Close() error { return nil }
