{
  description = "Authenticated Gnosis HTTP client for GNU Emacs";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
      mkGnosis =
        system:
        let
          pkgs = import nixpkgs { inherit system; };
          lib = pkgs.lib;
          versionLines = lib.filter (lib.hasPrefix ";; Version: ") (
            lib.splitString "\n" (builtins.readFile ./lisp/gnosis.el)
          );
          version =
            assert lib.assertMsg (
              lib.length versionLines == 1
            ) "lisp/gnosis.el must contain exactly one Version header";
            lib.removePrefix ";; Version: " (lib.head versionLines);
          emacs = pkgs.emacs-nox or pkgs.emacs;
          emacsPackages = pkgs.emacsPackagesFor emacs;
          elispFiles = [
            "lisp/gnosis-completion.el"
            "lisp/gnosis-core.el"
            "lisp/gnosis-core-assets.el"
            "lisp/gnosis-edit.el"
            "lisp/gnosis.el"
            "lisp/gnosis-review.el"
          ];
          testFiles = [
            "tests/gnosis-package-smoke.el"
            "tests/gnosis-test-core.el"
            "tests/gnosis-test-core-assets.el"
            "tests/gnosis-test-openapi.el"
            "tests/gnosis-test-core-integration.el"
            "tests/gnosis-test-dashboard.el"
            "tests/gnosis-test-edit.el"
            "tests/gnosis-test-boundary.el"
            "tests/gnosis-test-review.el"
          ];
          packageFiles = [
            "Makefile"
            "docs/gnosis.org"
          ] ++ elispFiles;
          checkFiles = packageFiles ++ [
            "NEWS.org"
            "tools/gnosis-openapi-generator.el"
            "tools/run-temp-integration.sh"
          ] ++ testFiles;
          sourceFor =
            files:
            lib.fileset.toSource {
              root = ./.;
              fileset = lib.fileset.unions (map (file: ./. + "/${file}") files);
            };
          packageSource = sourceFor packageFiles;
          checkSource = sourceFor checkFiles;
          packageLint = emacsPackages.package-lint;
          devEmacs = emacsPackages.emacsWithPackages (_: [ packageLint ]);
          elispFileArgs = lib.concatStringsSep " " elispFiles;
          gnosisEl = emacsPackages.trivialBuild {
            pname = "gnosis";
            inherit version;
            src = packageSource;
            packageRequires = [ ];
            nativeBuildInputs = [ pkgs.gnumake pkgs.texinfo ];
            buildPhase = ''
              runHook preBuild
              emacs -l package -f package-initialize -L lisp --batch \
                --eval "(setq byte-compile-error-on-warn t)" \
                -f batch-byte-compile ${elispFileArgs}
              (cd lisp && emacs --batch \
                --eval "(loaddefs-generate \".\" \"gnosis-autoloads.el\")")
              env GNOSIS_ENV_WRAPPED=1 make doc
              runHook postBuild
            '';
            installPhase = ''
              runHook preInstall
              lispdir="$out/share/emacs/site-lisp"
              infodir="$out/share/info"
              mkdir -p "$lispdir" "$infodir"
              install -m444 lisp/*.el lisp/*.elc "$lispdir/"
              install -m444 docs/gnosis.info "$infodir/"
              runHook postInstall
            '';
            meta = with lib; {
              description = "Authenticated Gnosis HTTP client for GNU Emacs";
              homepage = "https://thanosapollo.org/projects/gnosis/";
              license = licenses.gpl3Plus;
              platforms = emacs.meta.platforms;
            };
          };
          emacsWithGnosis = emacsPackages.emacsWithPackages (_: [ gnosisEl ]);
          checkEnvironment = ''
            unset EMACSDATA EMACSDOC EMACSLOADPATH EMACSPATH GREP_OPTIONS
            export HOME="$TMPDIR/home"
            export XDG_CACHE_HOME="$TMPDIR/cache"
            export XDG_CONFIG_HOME="$TMPDIR/config"
            export XDG_DATA_HOME="$TMPDIR/share"
            export XDG_STATE_HOME="$TMPDIR/state"
            export GNOSIS_ENV_WRAPPED=1
            mkdir -p "$HOME" "$XDG_CACHE_HOME" "$XDG_CONFIG_HOME" \
              "$XDG_DATA_HOME" "$XDG_STATE_HOME"
          '';
          mkCheck =
            name: target:
            pkgs.stdenvNoCC.mkDerivation {
              pname = "gnosis-${name}";
              inherit version;
              src = checkSource;
              nativeBuildInputs = [ devEmacs pkgs.gnumake pkgs.texinfo ];
              dontConfigure = true;
              buildPhase = ''
                runHook preBuild
                ${checkEnvironment}
                make EMACS=emacs ${target}
                runHook postBuild
              '';
              installPhase = ''
                mkdir -p "$out"
                touch "$out/${name}-passed"
              '';
            };
          smokeCheck = pkgs.stdenvNoCC.mkDerivation {
            pname = "gnosis-package-smoke";
            inherit version;
            src = checkSource;
            nativeBuildInputs = [ devEmacs ];
            dontConfigure = true;
            buildPhase = ''
              runHook preBuild
              ${checkEnvironment}
              installDir="${gnosisEl}/share/emacs/site-lisp"
              emacs --batch -q -L "$installDir" -L tests \
                --eval "(require 'gnosis-package-smoke)" \
                -f ert-run-tests-batch-and-exit
              runHook postBuild
            '';
            installPhase = ''
              mkdir -p "$out"
              touch "$out/smoke-passed"
            '';
          };
          mkApp = name: target: {
            type = "app";
            program = "${
              pkgs.writeShellApplication {
                name = "gnosis-${name}";
                runtimeInputs = [ devEmacs pkgs.gnumake pkgs.texinfo ];
                text = ''
                  exec env GNOSIS_ENV_WRAPPED=1 make EMACS=emacs ${target} "$@"
                '';
              }
            }/bin/gnosis-${name}";
          };
          testCheck = mkCheck "test" "test";
          lintCheck = mkCheck "lint" "lint";
        in
        {
          inherit
            devEmacs
            emacsWithGnosis
            gnosisEl
            lintCheck
            packageLint
            pkgs
            smokeCheck
            testCheck
            version
            ;
          apps = {
            default = mkApp "test" "test";
            doc = mkApp "doc" "doc";
            lint = mkApp "lint" "lint";
            test = mkApp "test" "test";
          };
        };
      mkDevShell =
        gnosis:
        gnosis.pkgs.mkShell {
          packages = with gnosis.pkgs; [
            gnosis.devEmacs
            git
            gnumake
            texinfo
          ];
          EMACS = "emacs";
          GNOSIS_ENV_WRAPPED = "1";
        };
    in
    {
      apps = forAllSystems (system: (mkGnosis system).apps);
      checks = forAllSystems (system: {
        default = (mkGnosis system).testCheck;
        lint = (mkGnosis system).lintCheck;
        package = (mkGnosis system).gnosisEl;
        smoke = (mkGnosis system).smokeCheck;
      });
      devShells = forAllSystems (system: {
        default = mkDevShell (mkGnosis system);
      });
      formatter = forAllSystems (system: (mkGnosis system).pkgs.nixfmt);
      overlays.default = final: prev: {
        gnosis = self.packages.${prev.system}.gnosis;
        emacs-with-gnosis = self.packages.${prev.system}.emacs-with-gnosis;
      };
      packages = forAllSystems (system: {
        default = (mkGnosis system).gnosisEl;
        emacs-with-gnosis = (mkGnosis system).emacsWithGnosis;
        gnosis = (mkGnosis system).gnosisEl;
      });
    };
}
