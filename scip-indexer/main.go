//! Go SCIP indexer using go/packages + go/types.
//!
//! Produces an `index.scip` file by analyzing Go source code through the
//! standard library's type-checking infrastructure. This is the first
//! language-specific SCIP generator for CodeTrail.
//!
//! Usage: go run scip-indexer/main.go <project_root> [--output index.scip]

package main

import (
	"encoding/json"
	"flag"
	"fmt"
	"go/types"
	"os"
	"path/filepath"
	"strings"

	"golang.org/x/tools/go/packages"
)

// SCIP-like output types (simplified for CodeTrail import)
type Output struct {
	ToolInfo  ToolInfo    `json:"toolInfo"`
	Documents []Document  `json:"documents"`
	Symbols   []SymbolInfo `json:"symbols"`
}

type ToolInfo struct {
	Name    string `json:"name"`
	Version string `json:"version"`
}

type Document struct {
	RelativePath string       `json:"relativePath"`
	Language     string       `json:"language"`
	Occurrences  []Occurrence `json:"occurrences"`
}

type Occurrence struct {
	Symbol      string  `json:"symbol"`
	SymbolRoles int32   `json:"symbolRoles"` // 1=definition, 0=reference
	Range       []int32 `json:"range"`       // [startLine, startCol, endLine, endCol]
}

type SymbolInfo struct {
	Symbol      string `json:"symbol"`
	DisplayName string `json:"displayName"`
	Kind        string `json:"kind"`
}

func main() {
	outputPath := flag.String("output", "index.scip.json", "output file path")
	flag.Parse()

	if flag.NArg() < 1 {
		fmt.Fprintf(os.Stderr, "Usage: go run main.go <project_root> [--output file]\n")
		os.Exit(1)
	}
	projectRoot := flag.Arg(0)

	cfg := &packages.Config{
		Mode: packages.NeedName | packages.NeedFiles | packages.NeedCompiledGoFiles |
			packages.NeedImports | packages.NeedTypes | packages.NeedTypesInfo |
			packages.NeedSyntax | packages.NeedModule,
		Dir: projectRoot,
	}

	pkgs, err := packages.Load(cfg, "./...")
	if err != nil {
		fmt.Fprintf(os.Stderr, "failed to load packages: %v\n", err)
		os.Exit(1)
	}

	if packages.PrintErrors(pkgs) > 0 {
		fmt.Fprintf(os.Stderr, "warning: some packages had errors\n")
	}

	output := Output{
		ToolInfo: ToolInfo{Name: "codetrail-go-indexer", Version: "0.1.0"},
	}

	symbolID := 0
	symbolMap := make(map[string]string) // qualifiedName -> symbolId
	docMap := make(map[string]*Document)

	for _, pkg := range pkgs {
		if pkg.TypesInfo == nil {
			continue
		}

		// Collect definitions
		for ident, obj := range pkg.TypesInfo.Defs {
			if obj == nil || !obj.Exported() {
				continue
			}
			pos := pkg.Fset.Position(ident.Pos())
			if !pos.IsValid() || !strings.HasPrefix(pos.Filename, projectRoot) {
				continue
			}
			relPath, _ := filepath.Rel(projectRoot, pos.Filename)

			qname := qualifiedName(obj)
			symID := fmt.Sprintf("local %d", symbolID)
			symbolID++
			symbolMap[qname] = symID

			doc := getOrCreateDoc(docMap, relPath)
			doc.Occurrences = append(doc.Occurrences, Occurrence{
				Symbol:      symID,
				SymbolRoles: 1, // definition
				Range:       []int32{int32(pos.Line - 1), int32(pos.Column - 1), int32(pos.Line - 1), int32(pos.Column - 1 + len(ident.Name))},
			})

			kind := objectKind(obj)
			output.Symbols = append(output.Symbols, SymbolInfo{
				Symbol:      symID,
				DisplayName: ident.Name,
				Kind:        kind,
			})
		}

		// Collect references
		for ident, obj := range pkg.TypesInfo.Uses {
			if obj == nil || !obj.Exported() {
				continue
			}
			qname := qualifiedName(obj)
			symID, ok := symbolMap[qname]
			if !ok {
				continue
			}
			pos := pkg.Fset.Position(ident.Pos())
			if !pos.IsValid() || !strings.HasPrefix(pos.Filename, projectRoot) {
				continue
			}
			relPath, _ := filepath.Rel(projectRoot, pos.Filename)

			doc := getOrCreateDoc(docMap, relPath)
			doc.Occurrences = append(doc.Occurrences, Occurrence{
				Symbol:      symID,
				SymbolRoles: 0, // reference
				Range:       []int32{int32(pos.Line - 1), int32(pos.Column - 1), int32(pos.Line - 1), int32(pos.Column - 1 + len(ident.Name))},
			})
		}
	}

	// Collect documents
	for _, doc := range docMap {
		output.Documents = append(output.Documents, *doc)
	}

	// Write output
	f, err := os.Create(*outputPath)
	if err != nil {
		fmt.Fprintf(os.Stderr, "failed to create output: %v\n", err)
		os.Exit(1)
	}
	defer f.Close()

	enc := json.NewEncoder(f)
	enc.SetIndent("", "  ")
	if err := enc.Encode(output); err != nil {
		fmt.Fprintf(os.Stderr, "failed to write output: %v\n", err)
		os.Exit(1)
	}

	fmt.Printf("Generated %s with %d documents, %d symbols\n", *outputPath, len(output.Documents), len(output.Symbols))
}

func getOrCreateDoc(m map[string]*Document, path string) *Document {
	if d, ok := m[path]; ok {
		return d
	}
	d := &Document{RelativePath: path, Language: "go"}
	m[path] = d
	return d
}

func qualifiedName(obj types.Object) string {
	if obj.Pkg() != nil {
		return obj.Pkg().Path() + "." + obj.Name()
	}
	return obj.Name()
}

func objectKind(obj types.Object) string {
	switch obj.(type) {
	case *types.Func:
		return "Function"
	case *types.Var:
		return "Variable"
	case *types.Const:
		return "Constant"
	case *types.TypeName:
		return "Type"
	default:
		return "Unknown"
	}
}
