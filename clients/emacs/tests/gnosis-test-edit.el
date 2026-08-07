;;; gnosis-test-edit.el --- Thema editor tests  -*- lexical-binding: t; -*-

;;; Code:

(require 'cl-lib)
(require 'ert)
(require 'gnosis-edit)

(defconst gnosis-test-edit--request
  `(:kind "basic" :keimenon "Question\nline two"
    :hypothesis ["guess" ,gnosis-core--json-null]
    :answer ["first" "second\nline"]
    :source_guid ,gnosis-core--json-null
    :parathema "Context" :review_image ,gnosis-core--json-null
    :tags ["logic"]
    :concepts [(:node_id "opaque/node?id" :relation "primary")]
    :asset_paths ["assets/abc/image.png"])
  "Complete thema request used by editor tests.")

(ert-deftest gnosis-edit-json-round-trip-is-lossless ()
  (with-temp-buffer
    (insert (gnosis-edit--render gnosis-test-edit--request))
    (should (equal gnosis-test-edit--request (gnosis-edit--parse)))))

(ert-deftest gnosis-edit-rejects-duplicate-json-fields ()
  (with-temp-buffer
    (insert "{\"kind\":\"basic\",\"kind\":\"cloze\"}")
    (should-error (gnosis-edit--parse) :type 'user-error)))

(ert-deftest gnosis-add-thema-opens-shared-text-editor ()
  (dolist (buffer (list gnosis-edit-new-buffer-name gnosis-edit-buffer-name))
    (when-let* ((live (get-buffer buffer))) (kill-buffer live)))
  (unwind-protect
      (cl-letf (((symbol-function 'gnosis-core--authenticated-authority)
                 (lambda (&optional _) 'opening-authority)))
        (save-window-excursion (gnosis-add-thema)))
    (with-current-buffer gnosis-edit-new-buffer-name
      (should (derived-mode-p 'gnosis-edit-mode))
      (should-not gnosis-edit--thema-id)
      (should-not gnosis-edit--revision)
      (should (eq 'opening-authority gnosis-edit--authority))
      (should (equal "basic" (plist-get (gnosis-edit--parse) :kind))))
    (when-let* ((buffer (get-buffer gnosis-edit-new-buffer-name)))
      (kill-buffer buffer))))

(ert-deftest gnosis-edit-mode-uses-js-json-mode ()
  (with-temp-buffer
    (gnosis-edit-mode)
    (should (derived-mode-p 'js-json-mode))))

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

(ert-deftest gnosis-edit-save-promotes-create-after-valid-response ()
  (let (calls callback (operation-calls 0))
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
                            append (list key value)))))
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
      (should-not (buffer-modified-p)))))

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
