// Copyright 2020 Ant Group. All rights reserved.
//
// SPDX-License-Identifier: Apache-2.0

package build

import (
	"io"
	"os"
	"os/exec"
	"strings"

	"github.com/sirupsen/logrus"
)

type BuilderOption struct {
	ParentBootstrapPath string
	ChunkDict           string
	BootstrapPath       string
	RootfsPath          string
	BackendType         string
	BackendConfig       string
	WhiteoutSpec        string
	OutputJSONPath      string
	PrefetchPatterns    string
	// A regular file or fifo into which commands nydus-image to dump contents.
	BlobPath     string
	AlignedChunk bool
}

type CompactOption struct {
	ChunkDict           string
	BootstrapPath       string
	OutputBootstrapPath string
	BackendType         string
	BackendConfigPath   string
	OutputJSONPath      string
	CompactConfigPath   string
}

type Builder struct {
	binaryPath string
	stdout     io.Writer
	stderr     io.Writer
}

func NewBuilder(binaryPath string) *Builder {
	return &Builder{
		binaryPath: binaryPath,
		stdout:     os.Stdout,
		stderr:     os.Stderr,
	}
}

func (builder *Builder) run(args []string, prefetchPatterns string) error {
	logrus.Debugf("\tCommand: %s %s", builder.binaryPath, strings.Join(args[:], " "))

	cmd := exec.Command(builder.binaryPath, args...)
	cmd.Stdout = builder.stdout
	cmd.Stderr = builder.stderr
	cmd.Stdin = strings.NewReader(prefetchPatterns)

	if err := cmd.Run(); err != nil {
		logrus.WithError(err).Errorf("fail to run %v %+v", builder.binaryPath, args)
		return err
	}

	return nil
}

func (builder *Builder) Compact(option CompactOption) error {
	args := []string{
		"compact",
		"--bootstrap", option.BootstrapPath,
		"--config", option.CompactConfigPath,
		"--backend-type", option.BackendType,
		"--backend-config-file", option.BackendConfigPath,
		"--log-level", "info",
		"--output-json", option.OutputJSONPath,
	}
	if option.OutputBootstrapPath != "" {
		args = append(args, "--output-bootstrap", option.OutputBootstrapPath)
	}
	if option.ChunkDict != "" {
		args = append(args, "--chunk-dict", option.ChunkDict)
	}
	return builder.run(args, "")
}

// Run exec nydus-image CLI to build layer
func (builder *Builder) Run(option BuilderOption) error {
	var args []string
	if option.ParentBootstrapPath == "" {
		args = []string{
			"create",
		}
	} else {
		args = []string{
			"create",
			"--parent-bootstrap",
			option.ParentBootstrapPath,
		}
	}
	if option.AlignedChunk {
		args = append(args, "--aligned-chunk")
	}
	if option.ChunkDict != "" {
		args = append(args, "--chunk-dict", option.ChunkDict)
	}

	args = append(
		args,
		"--bootstrap",
		option.BootstrapPath,
		"--log-level",
		"warn",
		"--whiteout-spec",
		option.WhiteoutSpec,
		"--output-json",
		option.OutputJSONPath,
		"--blob",
		option.BlobPath,
		option.RootfsPath,
	)

	if len(option.PrefetchPatterns) > 0 {
		args = append(args, "--prefetch-policy", "fs")
	}

	return builder.run(args, option.PrefetchPatterns)
}
