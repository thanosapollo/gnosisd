;;; gnosis-test-edit.el --- Thema editor tests  -*- lexical-binding: t; -*-

;;; Code:

(require 'cl-lib)
(require 'ert)
(require 'gnosis-edit)

(defconst gnosis-test-edit--request
  `(:kind "basic" :keimenon "  Question  \n\\opening\n** Answer\n"
    :hypothesis ["guess" ,gnosis-core--json-null "" "hint\n\n"]
    :answer ["first" "second\nline\n" "@null" ""]
    :source_guid ,gnosis-core--json-null
    :parathema " Context " :review_image ""
    :tags ["logic" "  spaced  "]
    :concepts [(:node_id "primary/κόμβος/雪" :relation "primary")
               (:node_id "reference/node\nquoted" :relation "reference")
               (:node_id "supporting/node" :relation "supporting")
               (:node_id "second/primary" :relation "primary")]
    :asset_paths ["assets/abc/image.png" "assets/trailing "])
  "Complete thema request used by editor tests.")

(ert-deftest gnosis-edit-text-round-trip-is-lossless ()
  (with-temp-buffer
    (insert (gnosis-edit--render gnosis-test-edit--request))
    (let ((text (buffer-string)))
      (should (string-prefix-p "* Thema\n\n** Kind\n" text))
      (should (string-match-p (regexp-quote "\n** Concepts\n") text))
      (should (string-match-p "\\\\\\*\\* Answer" text))
      (should-not (string-match-p "\\`[[:space:]]*{" text))
      (should (equal gnosis-test-edit--request (gnosis-edit--parse))))))

(ert-deftest gnosis-edit-text-parser-rejects-unknown-headings ()
  (with-temp-buffer
    (insert (replace-regexp-in-string
             "^\\*\\* Kind$" "** Unknown"
             (gnosis-edit--render gnosis-test-edit--request)))
    (should-error (gnosis-edit--parse) :type 'user-error)))

(ert-deftest gnosis-edit-text-parser-rejects-malformed-list-items ()
  (with-temp-buffer
    (insert (replace-regexp-in-string
             "^- first$" "first"
             (gnosis-edit--render gnosis-test-edit--request)))
    (should-error (gnosis-edit--parse) :type 'user-error)))

(ert-deftest gnosis-edit-mode-protects-structure-not-values ()
  (with-temp-buffer
    (gnosis-edit-mode)
    (gnosis-edit--replace-request gnosis-test-edit--request)
    (goto-char (point-min))
    (should (get-text-property (point) 'read-only))
    (re-search-forward "^\\*\\* Keimenon$")
    (should (get-text-property (line-beginning-position) 'read-only))
    (forward-line 1)
    (should-not (get-text-property (point) 'read-only))
    (insert "editable ")))

(ert-deftest gnosis-edit-undo-stops-at-rendered-request ()
  (let ((buffer (generate-new-buffer " *gnosis-edit-undo*")))
    (unwind-protect
        (with-current-buffer buffer
          (buffer-enable-undo)
          (gnosis-edit-mode)
          (gnosis-edit--replace-request gnosis-test-edit--request)
          (let ((baseline (buffer-string)))
            (goto-char (point-min))
            (re-search-forward "^\\*\\* Keimenon$")
            (forward-line 1)
            (insert "edited ")
            (undo-boundary)
            (undo-start)
            (while (condition-case nil
                       (progn (undo-more 1) t)
                     (user-error nil)))
            (should (equal baseline (buffer-string)))
            (should (get-text-property (point-min) 'read-only))
            (should (equal gnosis-test-edit--request (gnosis-edit--parse)))))
      (kill-buffer buffer))))

(ert-deftest gnosis-add-thema-opens-shared-text-editor ()
  (dolist (buffer (list gnosis-edit-new-buffer-name gnosis-edit-buffer-name))
    (when-let* ((live (get-buffer buffer))) (kill-buffer live)))
  (unwind-protect
      (cl-letf (((symbol-function 'gnosis-edit--request-context)
                 (lambda () '(:base_url "https://a.example"
                              :token "token"
                              :authority opening-authority))))
        (save-window-excursion (gnosis-add-thema)))
    (with-current-buffer gnosis-edit-new-buffer-name
      (should (derived-mode-p 'gnosis-edit-mode))
      (should-not gnosis-edit--thema-id)
      (should-not gnosis-edit--revision)
      (should (eq 'opening-authority gnosis-edit--authority))
      (should (equal "basic" (plist-get (gnosis-edit--parse) :kind))))
    (when-let* ((buffer (get-buffer gnosis-edit-new-buffer-name)))
      (kill-buffer buffer))))

(ert-deftest gnosis-edit-mode-uses-text-mode ()
  (with-temp-buffer
    (gnosis-edit-mode)
    (should (derived-mode-p 'text-mode))
    (should outline-minor-mode)))

(ert-deftest gnosis-edit-thema-fetches-before-opening-buffer ()
  (when-let* ((buffer (get-buffer gnosis-edit-buffer-name))) (kill-buffer buffer))
  (cl-letf (((symbol-function 'gnosis-core--token)
             (lambda () "opening-token"))
            ((symbol-function 'gnosis-core--authenticated-authority)
             (lambda (&optional token)
               (should (equal "opening-token" token))
               'opening-authority))
            ((symbol-function 'gnosis-core-thema)
             (lambda (id)
               (should (equal "opaque/thema" id))
               (should (equal "opening-token" gnosis-core--token-override))
               (append '(:thema_id "opaque/thema" :revision 7)
                       gnosis-test-edit--request)))
            ((symbol-function 'gnosis-core-thema-write-view)
             (lambda (_) gnosis-test-edit--request)))
    (unwind-protect
        (save-window-excursion (gnosis-edit-thema "opaque/thema"))
      (with-current-buffer gnosis-edit-buffer-name
        (should (equal "opaque/thema" gnosis-edit--thema-id))
        (should (= 7 gnosis-edit--revision))
        (should (eq 'opening-authority gnosis-edit--authority))
        (should (equal gnosis-test-edit--request (gnosis-edit--parse))))
      (when-let* ((buffer (get-buffer gnosis-edit-buffer-name)))
        (kill-buffer buffer)))))

(ert-deftest gnosis-edit-fetch-pins-base-when-configuration-rotates ()
  (when-let* ((buffer (get-buffer gnosis-edit-buffer-name))) (kill-buffer buffer))
  (let* ((base-a "https://a.example")
         (base-b "https://b.example")
         (token (make-string 40 ?a))
         (gnosis-core-base-url base-a))
    (unwind-protect
        (cl-letf (((symbol-function 'gnosis-core--token)
                   (lambda ()
                     (setq gnosis-core-base-url base-b)
                     token))
                  ((symbol-function 'gnosis-core-thema)
                   (lambda (_)
                     (should (equal base-a gnosis-core--base-url-override))
                     (should (equal token gnosis-core--token-override))
                     (append '(:thema_id "opaque" :revision 7)
                             gnosis-test-edit--request)))
                  ((symbol-function 'gnosis-core-thema-write-view)
                   (lambda (_) gnosis-test-edit--request))
                  ((symbol-function 'pop-to-buffer-same-window) #'switch-to-buffer))
          (save-window-excursion (gnosis-edit-thema "opaque"))
          (with-current-buffer gnosis-edit-buffer-name
            (should
             (equal (vector base-a (secure-hash 'sha256 token))
                    gnosis-edit--authority))))
      (when-let* ((buffer (get-buffer gnosis-edit-buffer-name)))
        (kill-buffer buffer)))))

(ert-deftest gnosis-edit-mutations-pin-base-when-configuration-rotates ()
  (dolist (kind '(create update retry))
    (let* ((base-a "https://a.example")
           (base-b "https://b.example")
           (token (make-string 40 ?a))
           (authority (vector base-a (secure-hash 'sha256 token)))
           (gnosis-core-base-url base-a)
           (calls 0)
           (projection
            (append '(:thema_id "opaque" :revision 8)
                    gnosis-test-edit--request)))
      (with-temp-buffer
        (gnosis-edit-mode)
        (setq-local gnosis-edit--authority authority)
        (cl-letf (((symbol-function 'gnosis-core--token)
                   (lambda ()
                     (setq gnosis-core-base-url base-b)
                     token))
                  ((symbol-function 'gnosis-core-thema-create)
                   (lambda (&rest _)
                     (should (equal base-a gnosis-core--base-url-override))
                     (should (equal token gnosis-core--token-override))
                     (cl-incf calls)
                     projection))
                  ((symbol-function 'gnosis-core-thema-update)
                   (lambda (&rest _)
                     (should (equal base-a gnosis-core--base-url-override))
                     (should (equal token gnosis-core--token-override))
                     (cl-incf calls)
                     projection))
                  ((symbol-function 'gnosis-core-thema-write-view)
                   (lambda (_) gnosis-test-edit--request))
                  ((symbol-function 'quit-window) #'ignore))
          (pcase kind
            ('create
             (gnosis-edit--send-authorized
              "op:create" gnosis-test-edit--request nil nil))
            ('update
             (gnosis-edit--send-authorized
              "op:update" gnosis-test-edit--request "opaque" 7))
            ('retry
             (setq-local gnosis-edit--recovery
                         (list :operation-id "op:retry" :authority authority
                               :thema-id "opaque" :revision 7
                               :request gnosis-test-edit--request))
             (gnosis-edit-retry-uncertain))))
        (should (= 1 calls))))))

(ert-deftest gnosis-edit-thema-preserves-existing-unsaved-buffer ()
  (let ((buffer (get-buffer-create gnosis-edit-buffer-name))
        (fetches 0))
    (unwind-protect
        (with-current-buffer buffer
          (erase-buffer)
          (insert "unsaved")
          (set-buffer-modified-p t)
          (cl-letf (((symbol-function 'gnosis-core-thema)
                     (lambda (&rest _) (cl-incf fetches))))
            (should-error (gnosis-edit-thema "other") :type 'user-error))
          (should (= fetches 0))
          (should (equal "unsaved" (buffer-string))))
      (kill-buffer buffer))))

(ert-deftest gnosis-edit-open-commands-preserve-uncertain-recovery ()
  (dolist (case `((,gnosis-edit-new-buffer-name gnosis-add-thema)
                  (,gnosis-edit-buffer-name gnosis-edit-thema "other")))
    (let* ((name (car case))
           (buffer (get-buffer-create name))
           (recovery (list :operation-id "op:uncertain"
                           :thema-id "original" :revision 7
                           :request (copy-tree gnosis-test-edit--request t)))
           (fetches 0))
      (unwind-protect
          (with-current-buffer buffer
            (gnosis-edit-mode)
            (setq-local gnosis-edit--recovery recovery)
            (set-buffer-modified-p nil)
            (cl-letf (((symbol-function 'gnosis-core-thema)
                       (lambda (&rest _) (cl-incf fetches))))
              (should-error (apply (cadr case) (cddr case)) :type 'user-error))
            (should (equal recovery gnosis-edit--recovery))
            (should (= fetches 0)))
        (when (buffer-live-p buffer)
          (with-current-buffer buffer (setq gnosis-edit--recovery nil))
          (kill-buffer buffer))))))

(ert-deftest gnosis-edit-renamed-buffers-refuse-kill-with-recovery ()
  (dolist (name (list gnosis-edit-new-buffer-name gnosis-edit-buffer-name))
    (when-let ((existing (get-buffer name)))
      (with-current-buffer existing (setq gnosis-edit--recovery nil))
      (kill-buffer existing))
    (let* ((buffer (get-buffer-create name))
           (recovery (list :operation-id "op:uncertain"
                           :thema-id "original" :revision 7
                           :request (copy-tree gnosis-test-edit--request t))))
      (unwind-protect
          (progn
            (with-current-buffer buffer
              (gnosis-edit-mode)
              (setq-local gnosis-edit--recovery recovery)
              (rename-buffer (generate-new-buffer-name "*Renamed Gnosis*"))
              (set-buffer-modified-p nil))
            (should-not (kill-buffer buffer))
            (should (buffer-live-p buffer))
            (should (equal recovery
                           (buffer-local-value 'gnosis-edit--recovery buffer)))
            (with-current-buffer buffer (setq gnosis-edit--recovery nil)))
        (when (buffer-live-p buffer)
          (with-current-buffer buffer (setq gnosis-edit--recovery nil))
          (kill-buffer buffer))))))

(ert-deftest gnosis-edit-renamed-buffer-reinit-preserves-recovery-guard ()
  (dolist (mode '(gnosis-edit-mode fundamental-mode))
    (let* ((buffer (generate-new-buffer "*Gnosis Edit*"))
           (recovery (list :operation-id "op:uncertain"
                           :authority 'opening-authority
                           :thema-id "original" :revision 7
                           :request (copy-tree gnosis-test-edit--request t))))
      (unwind-protect
          (progn
            (with-current-buffer buffer
              (gnosis-edit-mode)
              (setq-local gnosis-edit--recovery recovery)
              (rename-buffer (generate-new-buffer-name "*Renamed Gnosis*"))
              (funcall mode)
              (set-buffer-modified-p nil)
              (should (equal recovery gnosis-edit--recovery))
              (should (memq #'gnosis-edit--allow-kill
                            kill-buffer-query-functions)))
            (should-not (kill-buffer buffer))
            (should (equal recovery
                           (buffer-local-value 'gnosis-edit--recovery buffer))))
        (when (buffer-live-p buffer)
          (with-current-buffer buffer (setq gnosis-edit--recovery nil))
          (kill-buffer buffer))))))

(ert-deftest gnosis-edit-retry-after-mode-reinit-uses-retained-authority ()
  (let* ((request (copy-tree gnosis-test-edit--request t))
         (recovery (list :operation-id "op:uncertain"
                         :authority 'opening-authority
                         :thema-id "original" :revision 7
                         :request request))
         arguments exited)
    (with-temp-buffer
      (gnosis-edit-mode)
      (setq-local gnosis-edit--recovery recovery)
      (fundamental-mode)
      (cl-letf (((symbol-function 'gnosis-core--token)
                 (lambda () "opening-token"))
                ((symbol-function 'gnosis-core--authenticated-authority)
                 (lambda (&optional _token) 'opening-authority))
                ((symbol-function 'gnosis-core-thema-update)
                 (lambda (&rest args)
                   (setq arguments args)
                   `(:thema_id "original" :thema_revision 8
                     :kind "basic" :keimenon "Question" :hypothesis ""
                     :answer "Answer" :tags [])))
                ((symbol-function 'gnosis-core-thema-write-view)
                 (lambda (_) request))
                ((symbol-function 'quit-window)
                 (lambda (&rest _) (setq exited t))))
        (gnosis-edit-retry-uncertain)))
    (should (equal arguments (list "original" "op:uncertain" 7 request)))
    (should exited)))

(ert-deftest gnosis-edit-retry-after-mode-reinit-rejects-authority-drift ()
  (let ((recovery (list :operation-id "op:uncertain"
                        :authority 'opening-authority
                        :thema-id "original" :revision 7
                        :request (copy-tree gnosis-test-edit--request t)))
        called)
    (with-temp-buffer
      (gnosis-edit-mode)
      (setq-local gnosis-edit--recovery recovery)
      (fundamental-mode)
      (cl-letf (((symbol-function 'gnosis-core--token)
                 (lambda () "changed-token"))
                ((symbol-function 'gnosis-core--authenticated-authority)
                 (lambda (&optional _token) 'changed-authority))
                ((symbol-function 'gnosis-core-thema-update)
                 (lambda (&rest _) (setq called t))))
        (should-error (gnosis-edit-retry-uncertain)
                      :type 'gnosis-core-authorization-error))
      (should-not called)
      (should (equal recovery gnosis-edit--recovery)))))

(ert-deftest gnosis-edit-renamed-buffer-confirmed-discard-clears-before-close ()
  (let ((buffer (generate-new-buffer "*Gnosis New*"))
        (recovery (list :operation-id "op:uncertain"
                        :request (copy-tree gnosis-test-edit--request t))))
    (unwind-protect
        (with-current-buffer buffer
          (gnosis-edit-mode)
          (setq-local gnosis-edit--recovery recovery)
          (rename-buffer (generate-new-buffer-name "*Renamed Gnosis*"))
          (cl-letf (((symbol-function 'yes-or-no-p) (lambda (&rest _) t))
                    ((symbol-function 'quit-window)
                     (lambda (&rest _)
                       (should-not gnosis-edit--recovery)
                       (kill-buffer (current-buffer)))))
            (gnosis-edit-discard-uncertain))
          (should-not (buffer-live-p buffer)))
      (when (buffer-live-p buffer)
        (with-current-buffer buffer (setq gnosis-edit--recovery nil))
        (kill-buffer buffer)))))

(ert-deftest gnosis-edit-renamed-buffer-declined-discard-preserves-recovery ()
  (let ((buffer (generate-new-buffer "*Gnosis Edit*"))
        (recovery (list :operation-id "op:uncertain"
                        :request (copy-tree gnosis-test-edit--request t))))
    (unwind-protect
        (with-current-buffer buffer
          (gnosis-edit-mode)
          (setq-local gnosis-edit--recovery recovery)
          (rename-buffer (generate-new-buffer-name "*Renamed Gnosis*"))
          (cl-letf (((symbol-function 'yes-or-no-p) (lambda (&rest _) nil))
                    ((symbol-function 'quit-window)
                     (lambda (&rest _) (ert-fail "quit-window called"))))
            (should-error (gnosis-edit-discard-uncertain) :type 'user-error))
          (should (buffer-live-p buffer))
          (should (equal recovery gnosis-edit--recovery)))
      (when (buffer-live-p buffer)
        (with-current-buffer buffer (setq gnosis-edit--recovery nil))
        (kill-buffer buffer)))))

(ert-deftest gnosis-edit-renamed-buffer-without-recovery-kills-normally ()
  (let ((buffer (generate-new-buffer "*Gnosis Edit*")))
    (with-current-buffer buffer
      (gnosis-edit-mode)
      (rename-buffer (generate-new-buffer-name "*Renamed Gnosis*")))
    (should (kill-buffer buffer))
    (should-not (buffer-live-p buffer))))

(ert-deftest gnosis-edit-save-promotes-create-and-exits-after-valid-response ()
  (let (calls callback exited (operation-calls 0))
    (with-temp-buffer
      (gnosis-edit-mode)
      (setq-local gnosis-edit--authority 'test-authority)
      (setq-local gnosis-edit--after-save
                  (lambda (projection) (setq callback projection)))
      (insert (gnosis-edit--render gnosis-test-edit--request))
      (cl-letf (((symbol-function 'gnosis-core-operation-id)
                 (lambda ()
                   (cl-incf operation-calls)
                   "op:save"))
                ((symbol-function 'gnosis-core--token)
                 (lambda () "test-token"))
                ((symbol-function 'gnosis-core--authenticated-authority)
                 (lambda (&optional _) 'test-authority))
                ((symbol-function 'gnosis-core--typed-request)
                 (lambda (&rest arguments)
                   (push arguments calls)
                   `(:status "applied" :format_version 1
                     :operation_id "op:save" :changed t
                     :authoritative
                     (:kind "thema"
                      :value ,(append '(:thema_id "created" :revision 1)
                                      gnosis-test-edit--request))
                     :cursor "cursor")))
                ((symbol-function 'gnosis-core-thema-write-view)
                 (lambda (projection)
                   (cl-loop for (key value) on projection by #'cddr
                            unless (memq key '(:thema_id :revision))
                            append (list key value))))
                ((symbol-function 'quit-window)
                 (lambda (&rest _) (setq exited t))))
        (gnosis-edit-save))
      (should (= 1 (length calls)))
      (should (= 1 operation-calls))
      (should
       (equal `("post_themata" nil nil ,gnosis-test-edit--request
                (("Idempotency-Key" . "op:save")))
              (car calls)))
      (should (equal "created" gnosis-edit--thema-id))
      (should (= 1 gnosis-edit--revision))
      (should (equal "created" (plist-get callback :thema_id)))
      (should exited)
      (should-not (buffer-modified-p)))))

(ert-deftest gnosis-edit-success-exits-when-after-save-signals ()
  (dolist (update '(nil t))
    (with-temp-buffer
      (gnosis-edit-mode)
      (setq-local gnosis-edit--thema-id (and update "thema-a"))
      (setq-local gnosis-edit--revision (and update 7))
      (setq-local gnosis-edit--authority 'opening-authority)
      (setq-local gnosis-edit--after-save
                  (lambda (_) (signal 'error '("callback failed"))))
      (insert (gnosis-edit--render gnosis-test-edit--request))
      (let ((projection
             `(:thema_id "thema-a" :revision 8
               :kind "basic" :keimenon "Question" :hypothesis ["guess"]
               :answer ["Answer"] :tags []))
            (exits 0))
        (cl-letf (((symbol-function 'gnosis-edit--request-context)
                   (lambda () '(:base_url "https://a.example"
                                :token "token" :authority opening-authority)))
                  ((symbol-function 'gnosis-core-operation-id)
                   (lambda () "op:save"))
                  ((symbol-function 'gnosis-core-thema-create)
                   (lambda (&rest _) projection))
                  ((symbol-function 'gnosis-core-thema-update)
                   (lambda (&rest _) projection))
                  ((symbol-function 'gnosis-core-thema-write-view)
                   (lambda (_) gnosis-test-edit--request))
                  ((symbol-function 'quit-window)
                   (lambda (&rest _) (cl-incf exits))))
          (should-error (gnosis-edit-save) :type 'error))
        (should (= 1 exits))
        (should (equal "thema-a" gnosis-edit--thema-id))
        (should (= 8 gnosis-edit--revision))
        (should-not gnosis-edit--recovery)))))

(ert-deftest gnosis-edit-save-rejects-missing-opening-authority ()
  (with-temp-buffer
    (gnosis-edit-mode)
    (insert (gnosis-edit--render gnosis-test-edit--request))
    (let ((mutations 0))
      (cl-letf (((symbol-function 'gnosis-core-thema-create)
                 (lambda (&rest _) (cl-incf mutations))))
        (should-error (gnosis-edit-save)
                      :type 'gnosis-core-authorization-error))
      (should (= 0 mutations)))))

(ert-deftest gnosis-edit-save-rejects-changed-opening-authority ()
  (with-temp-buffer
    (gnosis-edit-mode)
    (setq-local gnosis-edit--authority 'opening-authority)
    (insert (gnosis-edit--render gnosis-test-edit--request))
    (let ((mutations 0))
      (cl-letf (((symbol-function 'gnosis-core--token) (lambda () "live-token"))
                ((symbol-function 'gnosis-core--authenticated-authority)
                 (lambda (&optional _) 'changed-authority))
                ((symbol-function 'gnosis-core-thema-create)
                 (lambda (&rest _)
                   (cl-incf mutations))))
        (should-error (gnosis-edit-save)
                      :type 'gnosis-core-authorization-error))
      (should (= 0 mutations))
      (should (buffer-modified-p)))))

(ert-deftest gnosis-edit-conflict-preserves-buffer-and-revision ()
  (with-temp-buffer
    (gnosis-edit-mode)
    (setq-local gnosis-edit--authority 'test-authority)
    (setq-local gnosis-edit--thema-id "opaque/thema")
    (setq-local gnosis-edit--revision 7)
    (insert (gnosis-edit--render gnosis-test-edit--request))
    (let ((before (buffer-string))
          calls)
      (cl-letf (((symbol-function 'gnosis-core-operation-id)
                 (lambda () "op:update"))
                ((symbol-function 'gnosis-core--token)
                 (lambda () "test-token"))
                ((symbol-function 'gnosis-core--authenticated-authority)
                 (lambda (&optional _) 'test-authority))
                ((symbol-function 'gnosis-core-thema-update)
                 (lambda (&rest arguments)
                   (push arguments calls)
                   (signal 'gnosis-core-conflict '("stale")))))
        (should-error (gnosis-edit-save) :type 'gnosis-core-conflict))
      (should
       (equal (list (list "opaque/thema" "op:update" 7
                          gnosis-test-edit--request))
              calls))
      (should (= 7 gnosis-edit--revision))
      (should (equal before (buffer-string))))))

(ert-deftest gnosis-edit-uncertain-mutation-locks-blind-retry ()
  (with-temp-buffer
    (gnosis-edit-mode)
    (setq-local gnosis-edit--authority 'test-authority)
    (insert (gnosis-edit--render gnosis-test-edit--request))
    (let ((calls 0)
          operation-ids)
      (cl-letf (((symbol-function 'gnosis-core-operation-id)
                 (lambda ()
                   (push "op:uncertain" operation-ids)
                   "op:uncertain"))
                ((symbol-function 'gnosis-core--token)
                 (lambda () "test-token"))
                ((symbol-function 'gnosis-core--authenticated-authority)
                 (lambda (&optional _) 'test-authority))
                ((symbol-function 'gnosis-core-thema-create)
                 (lambda (operation-id _request)
                   (should (equal "op:uncertain" operation-id))
                   (cl-incf calls)
                   (signal 'gnosis-core-mutation-uncertain '("uncertain")))))
        (should-error (gnosis-edit-save)
                      :type 'gnosis-core-mutation-uncertain)
        (should-error (gnosis-edit-save) :type 'user-error))
      (should
       (equal (list :operation-id "op:uncertain"
                    :authority 'test-authority
                    :thema-id nil :revision nil
                    :request gnosis-test-edit--request)
              gnosis-edit--recovery))
      (should (equal '("op:uncertain") operation-ids))
      (should (= calls 1)))))

(ert-deftest gnosis-edit-uncertain-retry-uses-retained-identity-and-request ()
  (with-temp-buffer
    (gnosis-edit-mode)
    (setq-local gnosis-edit--thema-id "changed-target")
    (setq-local gnosis-edit--revision 99)
    (setq-local gnosis-edit--authority 'test-authority)
    (insert (gnosis-edit--render gnosis-test-edit--request))
    (let ((retained (copy-tree gnosis-test-edit--request t))
          arguments)
      (setq-local gnosis-edit--recovery
                  (list :operation-id "op:uncertain"
                        :thema-id "original-target" :revision 7
                        :request retained))
      (erase-buffer)
      (insert "{\"kind\":\"changed\"}")
      (cl-letf (((symbol-function 'gnosis-core--token)
                 (lambda () "test-token"))
                ((symbol-function 'gnosis-core--authenticated-authority)
                 (lambda (&optional _) 'test-authority))
                ((symbol-function 'gnosis-core-thema-update)
                 (lambda (thema-id operation-id revision request)
                   (setq arguments
                         (list thema-id operation-id revision request))
                   (append '(:thema_id "original-target" :revision 8) request)))
                ((symbol-function 'gnosis-core-thema-write-view)
                 (lambda (projection)
                   (cl-loop for (key value) on projection by #'cddr
                            unless (memq key '(:thema_id :revision))
                            append (list key value))))
                ((symbol-function 'message) #'ignore))
        (gnosis-edit-retry-uncertain))
      (should (equal (list "original-target" "op:uncertain" 7 retained)
                     arguments))
      (should-not gnosis-edit--recovery)
      (should (equal retained (gnosis-edit--parse))))))

(ert-deftest gnosis-edit-uncertain-retry-errors-preserve-complete-recovery ()
  (dolist (error '((gnosis-core-validation-error "local preflight")
                   (gnosis-core-protocol-error "malformed response")
                   (gnosis-core-authentication-error "unauthorized")))
    (with-temp-buffer
      (gnosis-edit-mode)
      (setq-local gnosis-edit--authority 'test-authority)
      (let ((recovery
             (list :operation-id "op:uncertain"
                   :thema-id "original-target" :revision 7
                   :request (copy-tree gnosis-test-edit--request t))))
        (setq-local gnosis-edit--recovery recovery)
        (cl-letf (((symbol-function 'gnosis-core--token)
                   (lambda () "test-token"))
                  ((symbol-function 'gnosis-core--authenticated-authority)
                   (lambda (&optional _) 'test-authority))
                  ((symbol-function 'gnosis-core-thema-update)
                   (lambda (&rest _) (signal (car error) (cdr error)))))
          (should-error (gnosis-edit-retry-uncertain) :type (car error)))
        (should (equal recovery gnosis-edit--recovery))))))

(ert-deftest gnosis-edit-cancel-preserves-uncertain-recovery-until-discard ()
  (with-temp-buffer
    (gnosis-edit-mode)
    (setq-local gnosis-edit--recovery
                (list :operation-id "op:uncertain"
                      :request (copy-tree gnosis-test-edit--request t)))
    (let (closed)
      (cl-letf (((symbol-function 'quit-window)
                 (lambda (&rest _) (setq closed t))))
        (should-error (gnosis-edit-cancel) :type 'user-error)
        (should gnosis-edit--recovery)
        (should-not closed)
        (cl-letf (((symbol-function 'yes-or-no-p) (lambda (&rest _) t)))
          (gnosis-edit-discard-uncertain))
        (should-not gnosis-edit--recovery)
        (should closed)))))

(provide 'gnosis-test-edit)
;;; gnosis-test-edit.el ends here
