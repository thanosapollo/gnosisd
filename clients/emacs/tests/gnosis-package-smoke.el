;;; gnosis-package-smoke.el --- Installed package smoke tests  -*- lexical-binding: t; -*-

;;; Code:

(require 'ert)
(require 'gnosis)
(require 'gnosis-completion)
(require 'gnosis-core)
(require 'gnosis-core-assets)
(require 'gnosis-edit)
(require 'gnosis-nodes)
(require 'gnosis-review)
(require 'gnosis-tl)

(ert-deftest gnosis-package-loads-eight-public-modules ()
  (dolist (feature '(gnosis gnosis-core gnosis-core-assets
                     gnosis-edit gnosis-nodes gnosis-review gnosis-completion
                     gnosis-tl))
    (should (featurep feature))))

(ert-deftest gnosis-package-exposes-foundation-entrypoints ()
  (dolist (command '(gnosis gnosis-add-thema gnosis-edit-thema
                    gnosis-review gnosis-review-topic
                    gnosis-review-resume gnosis-core-status
                    gnosis-core-openapi))
    (should (fboundp command))))

(ert-deftest gnosis-package-does-not-load-legacy-backends ()
  (dolist (feature '(gnosis-db gnosis-sqlite gnosis-algorithm
                     gnosis-schedule gnosis-history))
    (should-not (featurep feature))))

(provide 'gnosis-package-smoke)
;;; gnosis-package-smoke.el ends here
